/// WhisperLocalPlugin — local Speech-to-Text via whisper.cpp (Metal-accelerated on Apple Silicon).
///
/// Audio (any format) is first converted to 16 kHz mono WAV by ffmpeg, then fed to
/// whisper.cpp through the `whisper-rs` crate. No Python involved.
///
/// The ~2 GB model is **lazily loaded**: by default it is loaded into memory only on
/// the first transcription and unloaded again after a configurable idle period
/// (`idle_timeout_secs`, default 20 min). Set `load_at_startup: true` to load eagerly.
///
/// The model must be a GGML `.bin` file. Download with:
///   curl -L -o models/ggml-large-v3.bin \
///     https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use core_api::plugin::{Plugin, PluginContext};
use core_api::transcribe::{Transcribe, TranscribeRegistry};

/// Default idle timeout before the model is unloaded from memory (20 minutes).
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1200;
/// How often the eviction task checks for idleness.
const EVICTION_TICK: Duration = Duration::from_secs(60);

// ── LazyModel ─────────────────────────────────────────────────────────────────
//
// Shared, droppable home of the GGML weights. Both the plugin and the registered
// transcriber hold an `Arc<LazyModel>`; the eviction task holds a `Weak`. The
// weights live behind `ctx` and can be dropped at any time to reclaim ~2 GB — the
// transcriber keeps working because it reloads on demand via `ensure_loaded()`.

struct LazyModel {
    /// Path to the GGML `.bin` file. Behind a Mutex so a config change can swap it.
    path:      tokio::sync::Mutex<PathBuf>,
    /// Loaded model context — `None` until first use (or after eviction).
    ctx:       tokio::sync::Mutex<Option<Arc<WhisperContext>>>,
    /// Timestamp of the last transcription, used to decide idle eviction.
    last_used: std::sync::Mutex<std::time::Instant>,
}

impl LazyModel {
    fn new() -> Self {
        Self {
            path:      tokio::sync::Mutex::new(PathBuf::new()),
            ctx:       tokio::sync::Mutex::new(None),
            last_used: std::sync::Mutex::new(std::time::Instant::now()),
        }
    }

    /// Reset the idle timer to now.
    fn touch(&self) {
        *self.last_used.lock().unwrap() = std::time::Instant::now();
    }

    async fn is_loaded(&self) -> bool {
        self.ctx.lock().await.is_some()
    }

    /// Ensure the model is resident in memory and return a handle to it. Loads
    /// from `path` on first use (or after eviction); a no-op clone otherwise.
    /// The `ctx` lock is held across the load so concurrent first-callers wait
    /// for a single load instead of loading the weights twice — but the lock is
    /// released before the returned handle is used for inference.
    async fn ensure_loaded(&self) -> Result<Arc<WhisperContext>> {
        let mut guard = self.ctx.lock().await;

        if let Some(ctx) = guard.as_ref() {
            self.touch();
            return Ok(Arc::clone(ctx));
        }

        let model_path = self.path.lock().await.clone();
        anyhow::ensure!(
            model_path.exists(),
            "whisper_local: model file not found at '{}'. \
             Download a GGML model and set the path via the plugins API.",
            model_path.display()
        );
        let path_str = model_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("model path is not valid UTF-8"))?
            .to_string();

        info!(model = %model_path.display(), "whisper_local: loading model into memory");
        let whisper_ctx = tokio::task::spawn_blocking(move || {
            let mut params = WhisperContextParameters::default();
            params.use_gpu(true);
            WhisperContext::new_with_params(&path_str, params)
                .map_err(|e| anyhow::anyhow!("failed to load whisper model: {e:?}"))
        })
        .await??;

        let ctx = Arc::new(whisper_ctx);
        *guard = Some(Arc::clone(&ctx));
        self.touch();
        Ok(ctx)
    }

    /// Drop the in-memory model. The actual free runs on a blocking thread because
    /// whisper.cpp cleanup may touch the GPU. In-flight transcriptions hold their
    /// own `Arc`, so memory is only reclaimed once they finish.
    async fn unload(&self) {
        if let Some(ctx) = self.ctx.lock().await.take() {
            let in_flight = Arc::strong_count(&ctx) - 1;
            tokio::task::spawn_blocking(move || drop(ctx));
            debug!(in_flight, "whisper_local: model unloaded from memory");
        }
    }

    /// Unload the model if it has been idle for at least `timeout`.
    async fn evict_if_idle(&self, timeout: Duration) {
        if !self.is_loaded().await {
            return;
        }
        let idle = self.last_used.lock().unwrap().elapsed();
        if idle >= timeout {
            info!(idle_secs = idle.as_secs(), "whisper_local: idle timeout reached — unloading model");
            self.unload().await;
        }
    }
}

// ── WhisperLocalPlugin ────────────────────────────────────────────────────────

pub struct WhisperLocalPlugin {
    /// Shared, lazily-loaded model weights.
    model:                Arc<LazyModel>,
    /// BCP-47 language code. Shared with the registered transcriber so runtime
    /// changes take effect without re-registering.
    language:             Arc<tokio::sync::Mutex<String>>,
    /// Idle seconds before unload. `0` = never unload.
    idle_timeout_secs:    AtomicU64,
    /// Load the model eagerly in `start()` instead of on first use.
    load_at_startup:      AtomicBool,
    running:              AtomicBool,
    /// Kept so stop() can deregister without needing the full context.
    transcribe_registry:  tokio::sync::Mutex<Option<Arc<dyn TranscribeRegistry>>>,
    /// Background idle-eviction task; aborted on stop()/reconfigure.
    evictor:              tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl WhisperLocalPlugin {
    pub fn new() -> Self {
        Self {
            model:               Arc::new(LazyModel::new()),
            language:            Arc::new(tokio::sync::Mutex::new("auto".to_string())),
            idle_timeout_secs:   AtomicU64::new(DEFAULT_IDLE_TIMEOUT_SECS),
            load_at_startup:     AtomicBool::new(false),
            running:             AtomicBool::new(false),
            transcribe_registry: tokio::sync::Mutex::new(None),
            evictor:             tokio::sync::Mutex::new(None),
        }
    }

    /// (Re)start the idle-eviction task to match the current `idle_timeout_secs`.
    /// A timeout of `0` means "never unload" — any existing task is dropped and
    /// none is spawned.
    async fn respawn_evictor(&self) {
        if let Some(handle) = self.evictor.lock().await.take() {
            handle.abort();
        }

        let secs = self.idle_timeout_secs.load(Ordering::Relaxed);
        if secs == 0 {
            return;
        }
        let timeout = Duration::from_secs(secs);
        let model = Arc::downgrade(&self.model);

        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(EVICTION_TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                match model.upgrade() {
                    Some(m) => m.evict_if_idle(timeout).await,
                    None => break, // plugin dropped
                }
            }
        });
        *self.evictor.lock().await = Some(handle);
    }
}

impl Default for WhisperLocalPlugin {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl Plugin for WhisperLocalPlugin {
    fn id(&self)          -> &str { "whisper_local" }
    fn name(&self)        -> &str { "Whisper Local" }
    fn description(&self) -> &str { "Local STT via whisper.cpp (Metal-accelerated)" }
    fn is_running(&self) -> bool  { self.running.load(Ordering::Relaxed) }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "model": {
                    "type":        "string",
                    "title":       "Model path",
                    "description": "Path to GGML .bin file (e.g. models/ggml-large-v3.bin)"
                },
                "language": {
                    "type":        "string",
                    "title":       "Language",
                    "description": "BCP-47 code (e.g. 'it', 'en') or 'auto' for auto-detect",
                    "default":     "auto"
                },
                "load_at_startup": {
                    "type":        "boolean",
                    "title":       "Load at startup",
                    "description": "Load the model into memory when the plugin starts. \
                                    If false (default), the model is loaded lazily on the first transcription.",
                    "default":     false
                },
                "idle_timeout_secs": {
                    "type":        "integer",
                    "title":       "Idle unload timeout (seconds)",
                    "description": "Unload the model from memory after this many seconds of inactivity. 0 = never unload.",
                    "default":     1200
                }
            },
            "required": ["model"]
        })
    }

    fn as_any(&self) -> &dyn std::any::Any { self }

    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> { self }

    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()> {
        let new_model   = config["model"].as_str().unwrap_or("").to_string();
        let new_lang    = config["language"].as_str().unwrap_or("auto").to_string();
        let new_eager   = config["load_at_startup"].as_bool().unwrap_or(false);
        let new_timeout = config["idle_timeout_secs"].as_u64().unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
        let old_model   = self.model.path.lock().await.to_string_lossy().to_string();
        let is_running  = self.is_running();

        // Scalar settings that don't depend on lifecycle. Language is shared with
        // the live transcriber, so this takes effect immediately.
        self.load_at_startup.store(new_eager, Ordering::Relaxed);
        self.idle_timeout_secs.store(new_timeout, Ordering::Relaxed);
        *self.language.lock().await = new_lang;

        match (enabled, is_running) {
            (true, false) => {
                anyhow::ensure!(!new_model.is_empty(),
                    "whisper_local: cannot start — `model` is missing from config");
                *self.model.path.lock().await = PathBuf::from(&new_model);
                self.start(ctx).await?;
            }
            (false, true) => {
                self.stop().await?;
            }
            (true, true) => {
                if new_model != old_model {
                    info!("whisper_local: model path changed — clearing cached model");
                    *self.model.path.lock().await = PathBuf::from(&new_model);
                    self.model.unload().await;
                }
                // Pick up a possibly-changed idle timeout.
                self.respawn_evictor().await;
                // Honour load_at_startup turning on (or a fresh model path) eagerly.
                if self.load_at_startup.load(Ordering::Relaxed) && !self.model.is_loaded().await {
                    self.model.ensure_loaded().await?;
                }
            }
            (false, false) => {}
        }
        Ok(())
    }

    async fn start(&self, ctx: PluginContext) -> Result<()> {
        if self.running.load(Ordering::Relaxed) { return Ok(()); }

        // Redirect all whisper.cpp / ggml C-level log output through Rust callbacks.
        // Since we don't enable the `log_backend` or `tracing_backend` features,
        // the trampolines are no-ops — this silences all init/model-load chatter.
        // The call is idempotent (backed by std::sync::Once).
        whisper_rs::install_logging_hooks();

        // Validate the model path up front so config errors surface immediately,
        // even though the weights are not loaded until first use (lazy mode).
        let model_path = self.model.path.lock().await.clone();
        anyhow::ensure!(
            model_path.exists(),
            "whisper_local: model file not found at '{}'. \
             Download a GGML model and set the path via the plugins API.",
            model_path.display()
        );

        self.running.store(true, Ordering::Relaxed);

        // Register a lightweight transcriber that shares the lazy model + language;
        // it holds no strong reference to the weights, so eviction can free them.
        *self.transcribe_registry.lock().await = Some(Arc::clone(&ctx.transcribe_registry));
        ctx.transcribe_registry.register(Arc::new(WhisperLocalTranscriber {
            model:    Arc::clone(&self.model),
            language: Arc::clone(&self.language),
        })).await;

        if self.load_at_startup.load(Ordering::Relaxed) {
            self.model.ensure_loaded().await?;
            info!(model = %model_path.display(), "whisper_local plugin started (model preloaded)");
        } else {
            info!(model = %model_path.display(), "whisper_local plugin started (lazy — loads on first use)");
        }

        self.respawn_evictor().await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.running.store(false, Ordering::Relaxed);
        if let Some(handle) = self.evictor.lock().await.take() {
            handle.abort();
        }
        self.model.unload().await;
        info!("whisper_local plugin stopped");
        if let Some(reg) = self.transcribe_registry.lock().await.take() {
            reg.unregister("whisper_local").await;
        }
        Ok(())
    }
}

// ── WhisperLocalTranscriber ───────────────────────────────────────────────────
//
// Lightweight handle registered in TranscribeManager. Shares the plugin's lazy
// model and language via Arc, so it never pins the weights in memory and always
// reflects the current language. Loads the model on demand at transcription time.

struct WhisperLocalTranscriber {
    model:    Arc<LazyModel>,
    language: Arc<tokio::sync::Mutex<String>>,
}

#[async_trait]
impl Transcribe for WhisperLocalTranscriber {
    fn id(&self) -> &str { "whisper_local" }

    async fn transcribe(&self, audio: Vec<u8>, format: &str) -> Result<String> {
        debug!(bytes = audio.len(), format, "whisper_local: transcribing audio");

        // Load on demand (no-op if already resident) and refresh the idle timer.
        let ctx      = self.model.ensure_loaded().await?;
        let language = self.language.lock().await.clone();

        let pcm = audio_to_pcm_f32(audio, format).await?;

        let text = tokio::task::spawn_blocking(move || {
            let mut state = ctx.create_state()
                .map_err(|e| anyhow::anyhow!("whisper state error: {e:?}"))?;

            let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
            params.set_language(Some(&language));
            params.set_print_special(false);
            params.set_print_progress(false);
            params.set_print_realtime(false);
            params.set_print_timestamps(false);

            state.full(params, &pcm)
                .map_err(|e| anyhow::anyhow!("whisper inference error: {e:?}"))?;

            let n = state.full_n_segments();

            let text = (0..n)
                .map(|i| {
                    state.get_segment(i)
                        .ok_or_else(|| anyhow::anyhow!("whisper: segment {i} out of range"))
                        .and_then(|s| s.to_str()
                            .map(str::to_string)
                            .map_err(|e| anyhow::anyhow!("whisper segment text error: {e:?}")))
                })
                .collect::<Result<Vec<_>>>()?
                .join(" ")
                .trim()
                .to_string();

            info!(chars = text.len(), segments = n, "whisper_local: transcription complete");
            Ok::<String, anyhow::Error>(text)
        })
        .await??;

        // Reset the idle timer again so a long inference isn't evicted right after.
        self.model.touch();
        Ok(text)
    }
}

// ── Audio conversion ──────────────────────────────────────────────────────────
//
// Uses ffmpeg (assumed installed) to decode any audio format to 16 kHz mono WAV,
// then reads it with hound. whisper.cpp requires f32 PCM at exactly 16 kHz mono.

async fn audio_to_pcm_f32(audio: Vec<u8>, format: &str) -> Result<Vec<f32>> {
    let pid = std::process::id();
    let tmp_in  = std::env::temp_dir().join(format!("whisper_in_{pid}.{format}"));
    let tmp_out = std::env::temp_dir().join(format!("whisper_out_{pid}.wav"));

    tokio::fs::write(&tmp_in, &audio).await?;

    let result = tokio::process::Command::new("ffmpeg")
        .args([
            "-y", "-i", tmp_in.to_str().unwrap(),
            "-ar", "16000",
            "-ac", "1",
            tmp_out.to_str().unwrap(),
        ])
        .output()
        .await;

    tokio::fs::remove_file(&tmp_in).await.ok();

    let output = result?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("ffmpeg audio conversion failed: {stderr}");
    }

    let wav_bytes = tokio::fs::read(&tmp_out).await?;
    tokio::fs::remove_file(&tmp_out).await.ok();

    tokio::task::spawn_blocking(move || {
        let mut reader = hound::WavReader::new(std::io::Cursor::new(wav_bytes))?;
        let spec = reader.spec();

        let pcm: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Int => {
                let bits = spec.bits_per_sample;
                reader.samples::<i32>()
                    .map(|s| s.map(|v| v as f32 / (1i32 << (bits - 1)) as f32))
                    .collect::<Result<_, _>>()?
            }
            hound::SampleFormat::Float => {
                reader.samples::<f32>()
                    .collect::<Result<_, _>>()?
            }
        };

        if pcm.is_empty() {
            warn!("whisper_local: decoded PCM is empty");
        }
        Ok::<Vec<f32>, anyhow::Error>(pcm)
    })
    .await?
}
