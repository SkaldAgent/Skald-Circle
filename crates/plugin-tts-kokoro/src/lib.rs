//! Kokoro ONNX TTS plugin.
//!
//! On start, writes the embedded `kokoro_server.py` to `models/kokoro/`,
//! spawns it as a subprocess, reads the bound port from its stdout, then
//! registers itself as a [`TextToSpeech`] provider with the TTS manager.
//!
//! The subprocess downloads the Kokoro ONNX model files from GitHub releases
//! on first run (~310 MB + 27 MB) and caches them in `models/kokoro/`.
//! No API token required.
//!
//! # Config (stored in `plugins` SQLite table)
//!
//! ```json
//! {
//!   "voice": "if_sara",
//!   "lang":  "it",
//!   "speed": 1.0
//! }
//! ```
//!
//! | Field   | Values                                  | Default    |
//! |---------|-----------------------------------------|------------|
//! | `voice` | `"if_sara"` \| `"im_nicola"` \| …       | `"if_sara"` |
//! | `lang`  | `"it"` \| `"en-us"` \| `"en-gb"` \| …  | `"it"`     |
//! | `speed` | `0.5` – `2.0`                            | `1.0`      |

use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

const KOKORO_SERVER_PY: &str = include_str!("kokoro_server.py");

use core_api::plugin::{Plugin, PluginContext};
use core_api::tts::TextToSpeech;

const PLUGIN_ID:      &str = "kokoro_tts";
const MODEL_DIR:      &str = "models/kokoro";
const PROVIDER_ID:    &str = "kokoro_tts";
const SERVER_PY_NAME: &str = "kokoro_server.py";

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Debug)]
struct KokoroConfig {
    voice: String,
    lang:  String,
    speed: f64,
}

impl KokoroConfig {
    fn from_value(v: &Value) -> Self {
        Self {
            voice: v["voice"].as_str().unwrap_or("if_sara").to_string(),
            lang:  v["lang"].as_str().unwrap_or("it").to_string(),
            speed: v["speed"].as_f64().unwrap_or(1.0),
        }
    }
}

// ── KokoroSynthesiser ─────────────────────────────────────────────────────────

struct KokoroSynthesiser {
    port:    u16,
    config:  KokoroConfig,
    http:    reqwest::Client,
}

impl KokoroSynthesiser {
    fn new(port: u16, config: KokoroConfig) -> Self {
        Self { port, config, http: reqwest::Client::new() }
    }
}

#[async_trait]
impl TextToSpeech for KokoroSynthesiser {
    fn id(&self)          -> &str { PROVIDER_ID }
    fn name(&self)        -> &str { "Kokoro TTS" }
    fn description(&self) -> Option<&str> {
        Some("Local Kokoro ONNX TTS — lightweight, fast, multilingual. \
              Runs on CPU, no GPU required. ~310 MB model, auto-downloaded on first use.")
    }
    fn instructions(&self) -> Option<&str> {
        Some("\
Kokoro TTS produces natural spoken audio. \
Write as you would speak: short sentences, no markdown, no bullet points, no symbols. \
For Italian, use standard Italian orthography — accented letters are fine. \
Do not include URLs, file paths, or code snippets; describe them in words instead.")
    }

    async fn synthesize(&self, text: &str, instructions: Option<&str>) -> Result<Vec<u8>> {
        // instructions may carry a voice override as the first word (same convention as Orpheus).
        let voice = instructions
            .and_then(|s| s.split_whitespace().next())
            .map(str::to_owned);

        let url = format!("http://127.0.0.1:{}/synthesize", self.port);

        let resp = self.http
            .post(&url)
            .json(&json!({
                "text":  text,
                "voice": voice.as_deref().unwrap_or(&self.config.voice),
                "lang":  self.config.lang,
                "speed": self.config.speed,
            }))
            .send()
            .await
            .map_err(|e| anyhow!("kokoro_tts: request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            anyhow::bail!("kokoro_tts: server error {status}: {msg}");
        }

        Ok(resp.bytes().await.map(|b| b.to_vec())
            .map_err(|e| anyhow!("kokoro_tts: failed to read bytes: {e}"))?)
    }
}

// ── Plugin inner state ────────────────────────────────────────────────────────

struct Inner {
    child:       Child,
    port:        u16,
    config:      KokoroConfig,
    script_path: std::path::PathBuf,
}

// ── KokoroTtsPlugin ───────────────────────────────────────────────────────────

pub struct KokoroTtsPlugin {
    running: AtomicBool,
    inner:   Mutex<Option<Inner>>,
}

impl KokoroTtsPlugin {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            inner:   Mutex::new(None),
        }
    }

    async fn do_start(&self, config: &KokoroConfig, ctx: &PluginContext) -> Result<()> {
        std::fs::create_dir_all(MODEL_DIR)
            .context("kokoro_tts: failed to create model dir")?;

        let script_path = std::path::Path::new(MODEL_DIR).join(SERVER_PY_NAME);
        std::fs::write(&script_path, KOKORO_SERVER_PY)
            .context("kokoro_tts: failed to write embedded server script")?;

        let mut child = Command::new("python3")
            .args([
                script_path.to_str().unwrap(),
                "--model-dir",      MODEL_DIR,
                "--default-voice",  &config.voice,
                "--default-lang",   &config.lang,
                "--default-speed",  &config.speed.to_string(),
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("kokoro_tts: failed to spawn python3")?;

        // Log stderr in background so Python errors appear in Skald's log file.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!("kokoro_tts(py/err): {line}");
                }
            });
        }

        let stdout = child.stdout.take()
            .ok_or_else(|| anyhow!("kokoro_tts: no stdout from subprocess"))?;
        let mut lines = BufReader::new(stdout).lines();
        let port = loop {
            match lines.next_line().await? {
                None => anyhow::bail!("kokoro_tts: subprocess exited before printing port"),
                Some(line) => {
                    if let Some(p) = line.strip_prefix("PORT:") {
                        break p.trim().parse::<u16>()
                            .context("kokoro_tts: invalid port from subprocess")?;
                    }
                    info!("kokoro_tts(py): {line}");
                }
            }
        };

        info!(port, "kokoro_tts: python server ready");

        let synthesiser = Arc::new(KokoroSynthesiser::new(port, config.clone()));
        ctx.tts_registry.register(Arc::clone(&synthesiser) as _).await;

        self.running.store(true, Ordering::Relaxed);
        *self.inner.lock().await = Some(Inner {
            child,
            port,
            config: config.clone(),
            script_path,
        });

        Ok(())
    }

    async fn do_stop(&self, ctx: &PluginContext) {
        ctx.tts_registry.unregister(PROVIDER_ID).await;
        if let Some(mut inner) = self.inner.lock().await.take() {
            let _ = inner.child.kill().await;
            let _ = std::fs::remove_file(&inner.script_path);
        }
        self.running.store(false, Ordering::Relaxed);
        info!("kokoro_tts: stopped");
    }
}

#[async_trait]
impl Plugin for KokoroTtsPlugin {
    fn id(&self)          -> &str { PLUGIN_ID }
    fn name(&self)        -> &str { "Kokoro TTS" }
    fn description(&self) -> &str {
        "Local text-to-speech using Kokoro ONNX. Lightweight (~310 MB), fast on CPU, \
         multilingual (Italian, English, Japanese, Chinese, Spanish, French…). \
         No API token required — model is downloaded automatically on first use."
    }
    fn is_running(&self) -> bool { self.running.load(Ordering::Relaxed) }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "voice": {
                    "type": "string",
                    "enum": [
                        "if_sara", "im_nicola",
                        "af_heart", "af_bella", "af_nicole", "af_sarah", "af_sky",
                        "am_adam", "am_michael",
                        "bf_emma", "bf_isabella", "bm_george", "bm_lewis"
                    ],
                    "default": "if_sara",
                    "description": "Voice ID. Prefix: a=American, b=British, i=Italian, j=Japanese, z=Chinese; f=female, m=male."
                },
                "lang": {
                    "type": "string",
                    "enum": ["it", "en-us", "en-gb", "ja", "zh", "es", "fr", "hi", "pt-br", "ko"],
                    "default": "it",
                    "description": "Language code for phonemisation."
                },
                "speed": {
                    "type": "number",
                    "minimum": 0.5,
                    "maximum": 2.0,
                    "default": 1.0,
                    "description": "Speech speed multiplier."
                }
            }
        })
    }

    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()> {
        let new_cfg = KokoroConfig::from_value(&config);
        let is_running = self.is_running();

        let config_changed = self.inner.lock().await
            .as_ref()
            .map(|i| i.config != new_cfg)
            .unwrap_or(false);

        match (enabled, is_running) {
            (true, false) => self.do_start(&new_cfg, &ctx).await?,
            (false, true) => self.do_stop(&ctx).await,
            (true, true) if config_changed => {
                info!("kokoro_tts: config changed — restarting");
                self.do_stop(&ctx).await;
                self.do_start(&new_cfg, &ctx).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn start(&self, ctx: PluginContext) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        warn!("kokoro_tts: stop() called without ctx — cannot unregister from TtsManager");
        if let Some(mut inner) = self.inner.lock().await.take() {
            let _ = inner.child.kill().await;
            let _ = inner.child.wait().await;
        }
        self.running.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn runtime_status(&self) -> Option<Value> {
        let inner = self.inner.try_lock().ok()?;
        let inner = inner.as_ref()?;
        Some(json!({
            "port":  inner.port,
            "voice": inner.config.voice,
            "lang":  inner.config.lang,
            "speed": inner.config.speed,
        }))
    }

    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> { self }
}
