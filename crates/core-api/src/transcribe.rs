use std::sync::Arc;

use async_trait::async_trait;

// ── Record types (DB ↔ manager) ───────────────────────────────────────────────

/// Full model record, mirroring one row in `transcribe_models`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TranscribeModelRecord {
    pub id:          i64,
    pub provider_id: i64,
    pub model_id:    String,
    /// Display alias (also used as the transcriber `id()`).
    pub name:        String,
    /// BCP-47 language hint, e.g. `"it"`. `None` → auto-detect.
    pub language:    Option<String>,
    /// Lower number = tried first by `get()`.
    pub priority:    i32,
}

/// Remote model info returned by a provider's `list_transcribe_models()`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RemoteTranscribeModelInfo {
    pub id:          String,
    pub name:        String,
    pub description: Option<String>,
    /// BCP-47 language codes supported by this model (empty = unknown).
    pub languages:   Vec<String>,
}

/// Implemented by any provider that can convert audio bytes to text.
/// The `format` hint (e.g. `"ogg"`, `"mp3"`) is advisory.
#[async_trait]
pub trait Transcribe: Send + Sync {
    /// A stable, unique identifier for this provider (e.g. `"whisper_local"`).
    fn id(&self) -> &str;
    async fn transcribe(&self, audio: Vec<u8>, format: &str) -> anyhow::Result<String>;
}

/// Resolves the currently active [`Transcribe`] provider.
///
/// Implemented by `TranscribeManager` in the main crate. Plugins store
/// `Arc<dyn TranscribeProvider>` so they can resolve the active transcriber
/// per-call without holding a reference to `AppState`.
#[async_trait]
pub trait TranscribeProvider: Send + Sync {
    async fn get(&self) -> Option<Arc<dyn Transcribe>>;
}

/// Write-side of the transcribe manager: register and remove ephemeral providers.
///
/// Implemented by `TranscribeManager`. Plugins that provide their own STT
/// (e.g. `WhisperLocalPlugin`) use this to register themselves at start and
/// unregister at stop.
#[async_trait]
pub trait TranscribeRegistry: Send + Sync {
    async fn register(&self, provider: Arc<dyn Transcribe>);
    async fn unregister(&self, id: &str);
}
