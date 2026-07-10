/// OpenAiTtsSynthesiser — cloud Text-to-Speech via any OpenAI-compatible
/// audio speech endpoint (OpenAI, …).
///
/// Calls `POST {base_url}/audio/speech` with a JSON body.
/// Returns raw audio bytes in the configured `response_format` (default `mp3`).
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use tracing::{debug, info, warn};

use super::TextToSpeech;

// ── OpenAiTtsSynthesiser ──────────────────────────────────────────────────────

pub struct OpenAiTtsSynthesiser {
    /// Stable identifier, e.g. `"openai_tts_alloy"`.
    id:           String,
    base_url:     String,
    api_key:      String,
    model:        String,
    /// Voice/speaker name sent as the `voice` field. `None` ⇒ `alloy`.
    /// Provider-specific: OpenAI uses `alloy`/`echo`/`nova`/…; Gemini uses
    /// `Kore`/`Puck`/`Zephyr`/… — an unknown name may make the provider error.
    voice:        Option<String>,
    /// Default instructions (voice style, tone, speed). Overridable per call.
    instructions: Option<String>,
    /// Requested audio format (`response_format`). `None` ⇒ `mp3`.
    response_format: Option<String>,
    http:         reqwest::Client,
}

impl OpenAiTtsSynthesiser {
    pub fn new(
        id:              impl Into<String>,
        base_url:        impl Into<String>,
        api_key:         impl Into<String>,
        model:           impl Into<String>,
        voice:           Option<String>,
        instructions:    Option<String>,
        response_format: Option<String>,
    ) -> Self {
        Self {
            id:           id.into(),
            base_url:     base_url.into(),
            api_key:      api_key.into(),
            model:        model.into(),
            voice,
            instructions,
            response_format,
            http:         reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl TextToSpeech for OpenAiTtsSynthesiser {
    fn id(&self)            -> &str          { &self.id }
    fn name(&self)          -> &str          { &self.id }
    fn instructions(&self)  -> Option<&str>  { self.instructions.as_deref() }
    fn output_format(&self) -> &str          { self.response_format.as_deref().unwrap_or("mp3") }

    async fn synthesize(&self, text: &str, instructions: Option<&str>) -> Result<Vec<u8>> {
        let effective_instructions = instructions.or(self.instructions.as_deref());
        // `None` ⇒ provider default. Some models reject `mp3` and require a
        // specific value (e.g. Gemini TTS only accepts `pcm`).
        let response_format = self.response_format.as_deref().unwrap_or("mp3");

        debug!(
            chars = text.len(),
            model = %self.model,
            response_format,
            has_instructions = effective_instructions.is_some(),
            "openai_tts: synthesising",
        );

        let url = format!("{}/audio/speech", self.base_url.trim_end_matches('/'));

        // Voice from the model config (`tts_models.voice_id`), `alloy` if unset.
        // `voice` is required by the OpenAI schema. NOTE: `alloy` is an OpenAI
        // voice — providers like Gemini use their own names (`Kore`, `Puck`, …)
        // and may reject/500 on an unknown one. Logged below on error.
        let voice = self.voice.as_deref().unwrap_or("alloy");

        let mut body = serde_json::json!({
            "model": self.model,
            "input": text,
            "voice": voice,
            "response_format": response_format,
        });

        if let Some(instr) = effective_instructions {
            body["instructions"] = serde_json::Value::String(instr.to_string());
        }

        let resp = self.http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("X-Title", core_api::APP_NAME)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("openai_tts: request failed: {e}"))?;

        let status = resp.status();

        if !status.is_success() {
            // Read the body once, as text. For 5xx the standard `error.message`
            // is usually generic ("Internal Server Error") and the real cause sits
            // in the raw body — OpenRouter nests the upstream provider error under
            // `error.metadata`. Capturing the full body is the only way to see it.
            let raw    = resp.text().await.unwrap_or_default();
            let detail = extract_error_detail(&raw);
            warn!(
                %status,
                model = %self.model,
                voice,
                response_format,
                url = %url,
                response_body = %raw.chars().take(2000).collect::<String>(),
                "openai_tts: provider returned error",
            );
            anyhow::bail!("openai_tts: API error {status}: {detail}");
        }

        let audio = resp
            .bytes()
            .await
            .map_err(|e| anyhow!("openai_tts: failed to read audio bytes: {e}"))?
            .to_vec();

        info!(
            bytes = audio.len(),
            model = %self.model,
            "openai_tts: synthesis complete",
        );

        Ok(audio)
    }
}

/// Pull the most informative message out of an OpenAI/OpenRouter error body.
/// Prefers OpenRouter's upstream detail (`error.metadata.raw` / `provider_error`),
/// then the standard `error.message`, then the raw body itself. Falls back to a
/// placeholder for an empty body so the caller never reports a bare status code.
fn extract_error_detail(raw: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        let msg = v["error"]["message"].as_str().unwrap_or("").trim();
        // OpenRouter wraps the upstream provider's real error here on 5xx.
        let upstream = v["error"]["metadata"]["raw"].as_str()
            .or_else(|| v["error"]["metadata"]["provider_error"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        return match (msg.is_empty(), upstream) {
            (false, Some(up)) => format!("{msg} — upstream: {up}"),
            (false, None)     => msg.to_string(),
            (true,  Some(up)) => up.to_string(),
            (true,  None)     => raw.trim().to_string(),
        };
    }
    let t = raw.trim();
    if t.is_empty() { "<empty response body>".into() } else { t.into() }
}
