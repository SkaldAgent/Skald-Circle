/// OpenAiAudioTranscriber — cloud Speech-to-Text via any OpenAI-compatible
/// audio transcription endpoint (OpenAI, OpenRouter, …).
///
/// Calls `POST {base_url}/audio/transcriptions` with a multipart/form-data body.
/// No local model, no GPU, no ffmpeg — the provider handles everything server-side.
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tracing::{debug, info};

use super::Transcribe;

// ── OpenAiAudioTranscriber ────────────────────────────────────────────────────

pub struct OpenAiAudioTranscriber {
    /// Stable identifier, e.g. `"openrouter_whisper"` or `"openai_whisper"`.
    id:       String,
    base_url: String,
    api_key:  String,
    model:    String,
    /// BCP-47 language hint (e.g. `"it"`, `"en"`). `None` = let the model auto-detect.
    language: Option<String>,
    http:     reqwest::Client,
}

impl OpenAiAudioTranscriber {
    pub fn new(
        id:       impl Into<String>,
        base_url: impl Into<String>,
        api_key:  impl Into<String>,
        model:    impl Into<String>,
        language: Option<String>,
    ) -> Self {
        Self {
            id:       id.into(),
            base_url: base_url.into(),
            api_key:  api_key.into(),
            model:    model.into(),
            language,
            http:     reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Transcribe for OpenAiAudioTranscriber {
    fn id(&self) -> &str { &self.id }

    async fn transcribe(&self, audio: Vec<u8>, format: &str) -> Result<String> {
        debug!(
            bytes = audio.len(),
            format,
            model = %self.model,
            "openai_audio: transcribing",
        );

        let mime = mime_for_format(format);
        let filename = format!("audio.{format}");

        let file_part = reqwest::multipart::Part::bytes(audio)
            .file_name(filename)
            .mime_str(mime)
            .map_err(|e| anyhow!("invalid mime type '{mime}': {e}"))?;

        let mut form = reqwest::multipart::Form::new()
            .text("model", self.model.clone())
            .part("file", file_part);

        if let Some(lang) = &self.language {
            form = form.text("language", lang.clone());
        }

        let url = format!("{}/audio/transcriptions", self.base_url.trim_end_matches('/'));

        let resp = self.http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("X-Title", core_api::APP_NAME)
            .multipart(form)
            .send()
            .await
            .map_err(|e| anyhow!("openai_audio: request failed: {e}"))?;

        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| anyhow!("openai_audio: response parse failed: {e}"))?;

        if !status.is_success() {
            let msg = body["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            anyhow::bail!("openai_audio: API error {status}: {msg}");
        }

        let text = body["text"]
            .as_str()
            .ok_or_else(|| anyhow!("openai_audio: missing 'text' field in response"))?
            .trim()
            .to_string();

        info!(
            chars = text.len(),
            model = %self.model,
            "openai_audio: transcription complete",
        );

        Ok(text)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Maps a file extension to an appropriate MIME type for the multipart upload.
/// The OpenAI audio API accepts: mp3, mp4, mpeg, mpga, m4a, wav, webm, ogg.
fn mime_for_format(format: &str) -> &'static str {
    match format {
        "mp3" | "mpeg" | "mpga" => "audio/mpeg",
        "mp4" | "m4a"           => "audio/mp4",
        "wav"                   => "audio/wav",
        "webm"                  => "audio/webm",
        "ogg"                   => "audio/ogg",
        "flac"                  => "audio/flac",
        _                       => "application/octet-stream",
    }
}
