mod db;
pub mod manager;
pub mod openai_tts;

pub use core_api::tts::{TextToSpeech, TtsProvider, TtsRegistry};
pub use core_api::tts::{TtsModelRecord, RemoteTtsModelInfo};
pub use manager::TtsManager;

/// Public model metadata for API responses.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TtsModelInfo {
    pub id:            i64,
    pub provider_id:   i64,
    pub provider_name: String,
    pub model_id:      String,
    pub voice_id:      Option<String>,
    pub name:          String,
    pub description:   Option<String>,
    pub instructions:  Option<String>,
    /// Requested audio `response_format` (`None` ⇒ provider default `mp3`).
    pub response_format: Option<String>,
    pub priority:      i32,
    /// `true` for plugin-registered (ephemeral) providers — not editable via the UI.
    pub from_plugin:   bool,
}
