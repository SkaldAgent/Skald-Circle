use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use tracing::{debug, info};

use core_api::plugin::{Plugin, PluginContext};
use core_api::provider::{
    ApiProvider, LlmProviderRecord, ProviderField, ProviderUiMeta, RemoteLlmModelInfo,
    ServiceType,
};
use core_api::transcribe::{RemoteTranscribeModelInfo, Transcribe, TranscribeModelRecord};
use core_api::tts::{RemoteTtsModelInfo, TextToSpeech, TtsModelRecord};

// ── Constants ─────────────────────────────────────────────────────────────────

const EL_BASE_URL:      &str = "https://api.elevenlabs.io/v1";
const EL_DEFAULT_MODEL: &str = "eleven_multilingual_v2";

// ── TTS Synthesiser ───────────────────────────────────────────────────────────

/// ElevenLabsTtsSynthesiser — cloud TTS via the ElevenLabs v1 API.
///
/// Endpoint: `POST https://api.elevenlabs.io/v1/text-to-speech/{voice_id}`
/// Auth:     `xi-api-key` header (not Bearer).
pub struct ElevenLabsTtsSynthesiser {
    id:           String,
    api_key:      String,
    model_id:     String,
    voice_id:     String,
    instructions: Option<String>,
    http:         reqwest::Client,
}

impl ElevenLabsTtsSynthesiser {
    pub fn new(
        id:           impl Into<String>,
        api_key:      impl Into<String>,
        model_id:     impl Into<String>,
        voice_id:     Option<String>,
        instructions: Option<String>,
    ) -> Self {
        let model_id = model_id.into();
        // Legacy fallback: if no voice_id, treat model_id as voice_id and use default model.
        let (resolved_model, resolved_voice) = match voice_id {
            Some(v) => (model_id, v),
            None    => (EL_DEFAULT_MODEL.to_string(), model_id),
        };
        Self {
            id:       id.into(),
            api_key:  api_key.into(),
            model_id: resolved_model,
            voice_id: resolved_voice,
            instructions,
            http:     reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl TextToSpeech for ElevenLabsTtsSynthesiser {
    fn id(&self)           -> &str         { &self.id }
    fn name(&self)         -> &str         { &self.id }
    fn instructions(&self) -> Option<&str> { self.instructions.as_deref() }

    async fn synthesize(&self, text: &str, _instructions: Option<&str>) -> Result<Vec<u8>> {
        debug!(
            chars    = text.len(),
            voice_id = %self.voice_id,
            "elevenlabs_tts: synthesising",
        );

        let url = format!("{EL_BASE_URL}/text-to-speech/{}", self.voice_id);

        let body = serde_json::json!({
            "text":     text,
            "model_id": self.model_id,
        });

        let resp = self.http
            .post(&url)
            .header("xi-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("elevenlabs_tts: request failed: {e}"))?;

        let status = resp.status();

        if !status.is_success() {
            let err: serde_json::Value = resp.json().await.unwrap_or_default();
            let msg = err["detail"]["message"].as_str()
                .or_else(|| err["detail"].as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("elevenlabs_tts: API error {status}: {msg}");
        }

        let audio = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("elevenlabs_tts: failed to read audio bytes: {e}"))?
            .to_vec();

        info!(bytes = audio.len(), voice_id = %self.voice_id, "elevenlabs_tts: synthesis complete");
        Ok(audio)
    }
}

// ── Transcriber ───────────────────────────────────────────────────────────────

/// ElevenLabsTranscriber — cloud Speech-to-Text via the ElevenLabs Scribe API.
///
/// Endpoint: `POST https://api.elevenlabs.io/v1/speech-to-text`
/// Auth:     `xi-api-key` header (not Bearer).
pub struct ElevenLabsTranscriber {
    id:       String,
    api_key:  String,
    model_id: String,
    http:     reqwest::Client,
}

impl ElevenLabsTranscriber {
    pub fn new(
        id:       impl Into<String>,
        api_key:  impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            id:       id.into(),
            api_key:  api_key.into(),
            model_id: model_id.into(),
            http:     reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Transcribe for ElevenLabsTranscriber {
    fn id(&self) -> &str { &self.id }

    async fn transcribe(&self, audio: Vec<u8>, format: &str) -> Result<String> {
        debug!(
            bytes    = audio.len(),
            format   = %format,
            model_id = %self.model_id,
            "elevenlabs_transcribe: transcribing",
        );

        let url = format!("{EL_BASE_URL}/speech-to-text");

        let filename = format!("audio.{format}");
        let part = reqwest::multipart::Part::bytes(audio)
            .file_name(filename)
            .mime_str("audio/wav")?;

        let form = reqwest::multipart::Form::new()
            .text("model_id", self.model_id.clone())
            .part("file", part);

        let resp = self.http
            .post(&url)
            .header("xi-api-key", &self.api_key)
            .multipart(form)
            .send()
            .await
            .map_err(|e| anyhow!("elevenlabs_transcribe: request failed: {e}"))?;

        let status = resp.status();

        if !status.is_success() {
            let err: serde_json::Value = resp.json().await.unwrap_or_default();
            let msg = err["detail"]["message"].as_str()
                .or_else(|| err["detail"].as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("elevenlabs_transcribe: API error {status}: {msg}");
        }

        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| anyhow!("elevenlabs_transcribe: failed to parse response: {e}"))?;

        let text = body["text"]
            .as_str()
            .ok_or_else(|| anyhow!("elevenlabs_transcribe: missing 'text' in response"))?
            .to_string();

        info!(chars = text.len(), "elevenlabs_transcribe: done");
        Ok(text)
    }
}

// ── ApiProvider ───────────────────────────────────────────────────────────────

/// ElevenLabs supports TTS and Transcription only — no LLM chat/completion.
pub struct ElevenLabsProvider {
    http: reqwest::Client,
}

impl ElevenLabsProvider {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }

    async fn fetch_models(&self, api_key: &str) -> Result<serde_json::Value> {
        self.http
            .get("https://api.elevenlabs.io/v1/models")
            .header("xi-api-key", api_key)
            .send()
            .await
            .map_err(|e| anyhow!("ElevenLabs request failed: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow!("ElevenLabs error response: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("ElevenLabs response parse failed: {e}"))
    }
}

#[async_trait]
impl ApiProvider for ElevenLabsProvider {
    fn type_id(&self) -> &'static str { "elevenlabs" }
    fn display_name(&self) -> &'static str { "ElevenLabs" }
    fn supported_types(&self) -> &'static [ServiceType] {
        &[ServiceType::Tts, ServiceType::Transcribe]
    }

    async fn list_llm_models(&self, _record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        Ok(None)
    }

    async fn list_tts_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteTtsModelInfo>>> {
        let api_key = record.api_key.as_deref()
            .ok_or_else(|| anyhow!("provider '{}': api_key required for elevenlabs model listing", record.name))?;
        let resp = self.fetch_models(api_key).await?;
        let models = resp.as_array()
            .ok_or_else(|| anyhow!("unexpected ElevenLabs response shape"))?
            .iter()
            .filter(|m| m["can_do_text_to_speech"].as_bool().unwrap_or(false))
            .map(|m| {
                let id   = m["model_id"].as_str().unwrap_or("").to_string();
                let name = m["name"].as_str().unwrap_or(&id).to_string();
                let description = m["description"].as_str().map(str::to_string);
                let languages = m["languages"].as_array()
                    .map(|langs| langs.iter()
                        .filter_map(|l| l["language_id"].as_str().map(str::to_string))
                        .collect())
                    .unwrap_or_default();
                let cost_factor  = m["token_cost_factor"].as_f64();
                let instructions = elevenlabs_tts_instructions(&id);
                RemoteTtsModelInfo { id, name, description, languages, cost_factor, instructions }
            })
            .collect();
        Ok(Some(models))
    }

    async fn list_transcribe_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteTranscribeModelInfo>>> {
        let api_key = record.api_key.as_deref()
            .ok_or_else(|| anyhow!("provider '{}': api_key required for elevenlabs model listing", record.name))?;
        let resp = self.fetch_models(api_key).await?;
        let models = resp.as_array()
            .ok_or_else(|| anyhow!("unexpected ElevenLabs response shape"))?
            .iter()
            .filter(|m| m["can_do_voice_conversion"].as_bool().unwrap_or(false)
                || m["model_id"].as_str().map(|id| id.starts_with("scribe")).unwrap_or(false))
            .map(|m| {
                let id   = m["model_id"].as_str().unwrap_or("").to_string();
                let name = m["name"].as_str().unwrap_or(&id).to_string();
                let description = m["description"].as_str().map(str::to_string);
                let languages = m["languages"].as_array()
                    .map(|langs| langs.iter()
                        .filter_map(|l| l["language_id"].as_str().map(str::to_string))
                        .collect())
                    .unwrap_or_default();
                RemoteTranscribeModelInfo { id, name, description, languages }
            })
            .collect();
        Ok(Some(models))
    }

    fn build_tts(&self, record: &LlmProviderRecord, model: &TtsModelRecord) -> Option<Result<Arc<dyn TextToSpeech>>> {
        Some((|| {
            let api_key = record.api_key.clone()
                .with_context(|| format!("provider '{}': api_key required for elevenlabs", record.name))?;
            Ok(Arc::new(ElevenLabsTtsSynthesiser::new(
                &model.name, api_key, &model.model_id, model.voice_id.clone(), model.instructions.clone(),
            )) as Arc<dyn TextToSpeech>)
        })())
    }

    fn build_transcriber(&self, record: &LlmProviderRecord, model: &TranscribeModelRecord) -> Option<Result<Arc<dyn Transcribe>>> {
        Some((|| {
            let api_key = record.api_key.clone()
                .with_context(|| format!("provider '{}': api_key required for elevenlabs", record.name))?;
            Ok(Arc::new(ElevenLabsTranscriber::new(
                &model.name, api_key, &model.model_id,
            )) as Arc<dyn Transcribe>)
        })())
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      "elevenlabs",
            display_name: "ElevenLabs",
            description:  Some("Text-to-speech and transcription"),
            color:        "#f59e0b",
            icon:         "bi-waveform",
            fields: &[
                ProviderField { key: "api_key", label: "API Key", required: true, secret: true },
            ],
        }
    }
}

fn elevenlabs_tts_instructions(model_id: &str) -> Option<String> {
    match model_id {
        "eleven_multilingual_v2" | "eleven_turbo_v2_5" | "eleven_turbo_v2" | "eleven_flash_v2_5" | "eleven_flash_v2" => Some(
            "You can use the following markers in text to add expressiveness:\n\
             - <break time=\"0.5s\" /> — pause of given duration\n\
             - <phoneme alphabet=\"ipa\" ph=\"...\">word</phoneme> — explicit pronunciation\n\
             Laugh, cough, or sigh naturally by writing them as actions in parentheses, e.g. (laughs), (sighs), (coughs).\n\
             Emphasise a word with ALL CAPS or by repeating letters (e.g. sooo goood).\n\
             Keep sentences short for best pacing.".to_string()
        ),
        "eleven_monolingual_v1" => Some(
            "English-only model. Supports (laughs), (sighs), (coughs) for non-verbal sounds.\n\
             Use ALL CAPS for emphasis. Avoid non-English characters.".to_string()
        ),
        _ => None,
    }
}

// ── Plugin ────────────────────────────────────────────────────────────────────

pub struct ElevenLabsPlugin {
    running: Mutex<bool>,
}

impl ElevenLabsPlugin {
    pub fn new() -> Self {
        Self { running: Mutex::new(false) }
    }
}

#[async_trait]
impl Plugin for ElevenLabsPlugin {
    fn id(&self)          -> &str { "elevenlabs" }
    fn name(&self)        -> &str { "ElevenLabs" }
    fn description(&self) -> &str { "ElevenLabs TTS and transcription provider" }
    fn is_running(&self)  -> bool { *self.running.lock() }

    async fn start(&self, ctx: PluginContext) -> Result<()> {
        ctx.api_provider_registry.register_plugin(Arc::new(ElevenLabsProvider::new()));
        *self.running.lock() = true;
        Ok(())
    }

    async fn reload(&self, enabled: bool, _config: Value, ctx: PluginContext) -> Result<()> {
        if enabled {
            ctx.api_provider_registry.register_plugin(Arc::new(ElevenLabsProvider::new()));
            *self.running.lock() = true;
        } else {
            ctx.api_provider_registry.unregister_plugin("elevenlabs");
            *self.running.lock() = false;
        }
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        *self.running.lock() = false;
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> { self }
}
