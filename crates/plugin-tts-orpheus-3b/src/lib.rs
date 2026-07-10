//! Orpheus TTS 3B plugin.
//!
//! On start, writes the embedded `orpheus_server.py` (bundled via
//! `include_str!`) to `models/orpheus-3b/`, spawns it as a subprocess, reads
//! the bound port from its stdout, then registers itself as a [`TextToSpeech`]
//! provider with the TTS manager.
//!
//! The subprocess loads the Orpheus 3B model from HuggingFace (auto-download
//! on first run, cached in `models/orpheus-3b/`) and exposes a minimal HTTP
//! server on a random OS-assigned port.
//!
//! # Required secret
//!
//! Set before enabling the plugin:
//! ```
//! set_secret("HUGGINGFACE_TOKEN", "hf_...")
//! ```
//! Get a token at <https://huggingface.co/settings/tokens>.
//!
//! # Config (stored in `plugins` SQLite table)
//!
//! ```json
//! {
//!   "quantization": "int8",
//!   "voice": "tara"
//! }
//! ```
//!
//! | Field | Values | Default |
//! |-------|--------|---------|
//! | `quantization` | `"none"` \| `"int8"` \| `"int4"` | `"int8"` |
//! | `voice` | `"tara"` \| `"dan"` \| `"leah"` \| `"zac"` \| `"zoe"` \| `"mia"` \| `"julia"` \| `"leo"` | `"tara"` |

use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{info, warn};

const ORPHEUS_SERVER_PY: &str = include_str!("orpheus_server.py");

use core_api::plugin::{Plugin, PluginContext};
use core_api::secrets;
use core_api::tts::TextToSpeech;

const PLUGIN_ID:   &str = "orpheus_tts_3b";
const MODEL_DIR:   &str = "models/orpheus-3b";
const PROVIDER_ID: &str = "orpheus_tts_3b";
const SERVER_PY_NAME: &str = "orpheus_server.py";

// ── Config ────────────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Debug)]
struct OrpheusTtsConfig {
    quantization: String,
    voice:        String,
}

impl OrpheusTtsConfig {
    fn from_value(v: &Value) -> Self {
        Self {
            quantization: v["quantization"].as_str().unwrap_or("int8").to_string(),
            voice:        v["voice"].as_str().unwrap_or("tara").to_string(),
        }
    }
}

// ── OrpheusSynthesiser ────────────────────────────────────────────────────────

/// Calls the local Orpheus Python server to synthesise audio.
struct OrpheusSynthesiser {
    port:         u16,
    default_voice: String,
    http:         reqwest::Client,
}

impl OrpheusSynthesiser {
    fn new(port: u16, default_voice: impl Into<String>) -> Self {
        Self {
            port,
            default_voice: default_voice.into(),
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl TextToSpeech for OrpheusSynthesiser {
    fn id(&self)          -> &str { PROVIDER_ID }
    fn name(&self)        -> &str { "Orpheus TTS 3B" }
    fn description(&self) -> Option<&str> {
        Some("Local Orpheus TTS 3B — high-quality expressive speech, runs on-device.")
    }
    fn instructions(&self) -> Option<&str> {
        Some("\
Orpheus TTS supports inline emotion tags. Insert them directly in the text where the effect should occur.\n\
Supported tags: <laugh>, <chuckle>, <sigh>, <cough>, <sniffle>, <groan>, <yawn>, <gasp>\n\
Example: \"I told him the meeting was at nine, not eleven. <sigh> He showed up at noon. <chuckle> Classic.\"\
        ")
    }

    async fn synthesize(&self, text: &str, instructions: Option<&str>) -> Result<Vec<u8>> {
        let voice = instructions
            .and_then(|s| s.split_whitespace().next())  // first word as voice override
            .unwrap_or(&self.default_voice);

        let url = format!("http://127.0.0.1:{}/synthesize", self.port);

        let resp = self.http
            .post(&url)
            .json(&json!({
                "text":         text,
                "voice":        voice,
                "instructions": instructions,
            }))
            .send()
            .await
            .map_err(|e| anyhow!("orpheus_tts: request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            anyhow::bail!("orpheus_tts: server error {status}: {msg}");
        }

        Ok(resp.bytes().await.map(|b| b.to_vec())
            .map_err(|e| anyhow!("orpheus_tts: failed to read bytes: {e}"))?)
    }
}

// ── Plugin inner state ────────────────────────────────────────────────────────

struct Inner {
    child:      Child,
    port:       u16,
    config:     OrpheusTtsConfig,
    script_path: std::path::PathBuf,
}

// ── OrpheusTtsPlugin ──────────────────────────────────────────────────────────

pub struct OrpheusTtsPlugin {
    running: AtomicBool,
    inner:   Mutex<Option<Inner>>,
}

impl OrpheusTtsPlugin {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            inner:   Mutex::new(None),
        }
    }

    async fn do_start(&self, config: &OrpheusTtsConfig, ctx: &PluginContext) -> Result<()> {
        std::fs::create_dir_all(MODEL_DIR)
            .context("orpheus_tts: failed to create model dir")?;

        // Write the embedded script to the model dir so it can be executed.
        let script_path = std::path::Path::new(MODEL_DIR).join(SERVER_PY_NAME);
        std::fs::write(&script_path, ORPHEUS_SERVER_PY)
            .context("orpheus_tts: failed to write embedded server script")?;

        // HuggingFace token — required for gated repos. Passed as env var so
        // transformers/huggingface_hub pick it up automatically.
        let hf_token = secrets::require(&ctx.secrets, "HUGGINGFACE_TOKEN").await?;

        let mut child = Command::new("python3")
            .args([
                script_path.to_str().unwrap(),
                "--model-dir",     MODEL_DIR,
                "--quantization",  &config.quantization,
                "--default-voice", &config.voice,
            ])
            .env("HF_TOKEN", &hf_token)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .context("orpheus_tts: failed to spawn python3")?;

        // Read stdout until we see "PORT:<n>" — the server prints it once bound.
        let stdout = child.stdout.take()
            .ok_or_else(|| anyhow!("orpheus_tts: no stdout from subprocess"))?;
        let mut lines = BufReader::new(stdout).lines();
        let port = loop {
            match lines.next_line().await? {
                None => anyhow::bail!("orpheus_tts: subprocess exited before printing port"),
                Some(line) => {
                    if let Some(p) = line.strip_prefix("PORT:") {
                        break p.trim().parse::<u16>()
                            .context("orpheus_tts: invalid port from subprocess")?;
                    }
                    // Forward other startup lines as info.
                    info!("orpheus_tts(py): {line}");
                }
            }
        };

        info!(port, "orpheus_tts: python server ready");

        let synthesiser = Arc::new(OrpheusSynthesiser::new(port, &config.voice));
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
        info!("orpheus_tts: stopped");
    }
}

#[async_trait]
impl Plugin for OrpheusTtsPlugin {
    fn id(&self)          -> &str { PLUGIN_ID }
    fn name(&self)        -> &str { "Orpheus TTS 3B" }
    fn description(&self) -> &str {
        "Local text-to-speech using Orpheus 3B. Expressive, high-quality, runs fully on-device. \
         Requires ~7 GB VRAM (fp16), ~4 GB (int8), or ~2.5 GB (int4). \
         Requires secret: HUGGINGFACE_TOKEN (HuggingFace access token — \
         get one at https://huggingface.co/settings/tokens, then call \
         set_secret(\"HUGGINGFACE_TOKEN\", \"hf_...\")). \
         See docs/tts-providers.md for full setup instructions."
    }
    fn is_running(&self)  -> bool { self.running.load(Ordering::Relaxed) }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "quantization": {
                    "type": "string",
                    "enum": ["none", "int8", "int4"],
                    "default": "int8",
                    "description": "bitsandbytes precision: none=fp16 (~7 GB VRAM), int8 (~4 GB), int4 (~2.5 GB)"
                },
                "voice": {
                    "type": "string",
                    "enum": ["tara", "dan", "leah", "zac", "zoe", "mia", "julia", "leo"],
                    "default": "tara",
                    "description": "Default voice. Can be overridden per synthesis call via instructions."
                }
            }
        })
    }

    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()> {
        let new_cfg = OrpheusTtsConfig::from_value(&config);
        let is_running = self.is_running();

        let config_changed = self.inner.lock().await
            .as_ref()
            .map(|i| i.config != new_cfg)
            .unwrap_or(false);

        match (enabled, is_running) {
            (true, false) => self.do_start(&new_cfg, &ctx).await?,
            (false, true) => self.do_stop(&ctx).await,
            (true, true) if config_changed => {
                info!("orpheus_tts: config changed — restarting");
                self.do_stop(&ctx).await;
                self.do_start(&new_cfg, &ctx).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn start(&self, ctx: PluginContext) -> Result<()> {
        // start() is called by the plugin manager; reload() handles the normal path.
        // This is a no-op here — reload() does the real work.
        let _ = ctx;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        warn!("orpheus_tts: stop() called without ctx — cannot unregister from TtsManager");
        if let Some(mut inner) = self.inner.lock().await.take() {
            let _ = inner.child.kill().await;
        }
        self.running.store(false, Ordering::Relaxed);
        Ok(())
    }

    fn runtime_status(&self) -> Option<Value> {
        let inner = self.inner.try_lock().ok()?;
        let inner = inner.as_ref()?;
        Some(json!({
            "port":         inner.port,
            "quantization": inner.config.quantization,
            "voice":        inner.config.voice,
        }))
    }

    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> { self }
}
