use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

// ── Record types (DB ↔ manager) ───────────────────────────────────────────────

/// Full model record, mirroring one row in `tts_models`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TtsModelRecord {
    pub id:           i64,
    pub provider_id:  i64,
    pub model_id:     String,
    /// Voice/speaker identifier, if the provider requires it separately from the model.
    pub voice_id:     Option<String>,
    /// Display alias (also used as the synthesiser `id()`).
    pub name:         String,
    pub description:  Option<String>,
    /// Default voice instructions (tone, speed, style).
    pub instructions: Option<String>,
    /// Audio container/codec requested from the provider (`response_format`):
    /// `mp3`, `opus`, `aac`, `flac`, `wav`, `pcm`. `None` ⇒ provider default (`mp3`).
    /// Some models only accept a specific value (e.g. Gemini TTS requires `pcm`).
    pub response_format: Option<String>,
    /// Lower number = tried first by `get()`.
    pub priority:     i32,
}

/// Remote model info returned by a provider's `list_tts_models()`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RemoteTtsModelInfo {
    pub id:          String,
    pub name:        String,
    pub description: Option<String>,
    /// BCP-47 language codes supported by this model (empty = unknown).
    pub languages:    Vec<String>,
    /// Cost multiplier relative to the provider's base rate (1.0 = standard).
    pub cost_factor:  Option<f64>,
    /// Usage instructions: supported tags, markup, etc. Shown in UI and passed
    /// to the LLM when generating text destined for this synthesiser.
    pub instructions: Option<String>,
}

/// Implemented by any provider that can convert text to audio bytes.
/// Returns raw audio bytes (MP3 expected unless the provider states otherwise).
#[async_trait]
pub trait TextToSpeech: Send + Sync {
    /// A stable, unique identifier for this provider (e.g. `"openai_tts_alloy"`).
    fn id(&self) -> &str;
    /// Human-readable display name.
    fn name(&self) -> &str;
    /// Human-readable description (voice style, language, ideal use cases).
    fn description(&self) -> Option<&str> { None }
    /// Default synthesis instructions: voice style, tone, speed, and any
    /// provider-specific text markup syntax (e.g. emotion tags).
    /// Surfaced to the LLM via `TtsModelInfo` so it knows how to format input text.
    /// Individual call-time instructions passed to `synthesize` take precedence.
    fn instructions(&self) -> Option<&str> { None }
    /// Audio format (container/codec) of the bytes returned by `synthesize`,
    /// e.g. `mp3`, `opus`, `wav`, `pcm`. Consumers that require a specific
    /// container (e.g. Telegram voice messages need Ogg/Opus) use this to decide
    /// whether and how to transcode. Default `"mp3"`.
    fn output_format(&self) -> &str { "mp3" }
    /// Synthesise `text` to audio bytes.
    /// `instructions` overrides the provider's default instructions for this call only.
    async fn synthesize(&self, text: &str, instructions: Option<&str>) -> Result<Vec<u8>>;
}

/// Resolves the currently active [`TextToSpeech`] provider.
///
/// Implemented by `TtsManager` in the main crate. Plugins store
/// `Arc<dyn TtsProvider>` to resolve the active synthesiser per-call
/// without holding a reference to `AppState`.
#[async_trait]
pub trait TtsProvider: Send + Sync {
    async fn get(&self) -> Option<Arc<dyn TextToSpeech>>;
}

/// Write-side of the TTS manager: register and remove ephemeral providers.
///
/// Implemented by `TtsManager`. Plugins that supply their own TTS engine
/// (e.g. a local Kokoro or Piper plugin) use this to register at start
/// and unregister at stop.
#[async_trait]
pub trait TtsRegistry: Send + Sync {
    async fn register(&self, provider: Arc<dyn TextToSpeech>);
    async fn unregister(&self, id: &str);
}
