mod db;
pub mod manager;
pub mod openai_audio;

pub use core_api::transcribe::{Transcribe, TranscribeProvider, TranscribeRegistry};
pub use core_api::transcribe::{TranscribeModelRecord, RemoteTranscribeModelInfo};
pub use manager::TranscribeManager;

/// Public model metadata for API responses.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TranscribeModelInfo {
    pub id:            i64,
    pub provider_id:   i64,
    pub provider_name: String,
    pub model_id:      String,
    pub name:          String,
    pub language:      Option<String>,
    pub priority:      i32,
    /// `true` for plugin-registered (ephemeral) providers — not editable via the UI.
    pub from_plugin:   bool,
}
