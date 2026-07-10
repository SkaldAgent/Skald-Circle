/// OpenRouter image generation via the chat completions endpoint with `modalities`.
///
/// Calls `POST {base_url}/chat/completions` with:
///   `{"model": ..., "messages": [...], "modalities": ["image", "text"]}`
///
/// The response image is returned as a base64 data URL inside
/// `choices[0].message.images[0].image_url.url`.
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::Engine;
use tracing::{debug, info};

use super::ImageGenerate;

pub struct OpenRouterImageGenerator {
    /// Stable display identifier, e.g. `"my_openrouter_grok"`.
    id:       String,
    base_url: String,
    api_key:  String,
    model:    String,
    http:     reqwest::Client,
}

impl OpenRouterImageGenerator {
    pub fn new(
        id:       impl Into<String>,
        base_url: impl Into<String>,
        api_key:  impl Into<String>,
        model:    impl Into<String>,
    ) -> Self {
        Self {
            id:       id.into(),
            base_url: base_url.into(),
            api_key:  api_key.into(),
            model:    model.into(),
            http:     reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl ImageGenerate for OpenRouterImageGenerator {
    fn id(&self)   -> &str { &self.id }
    fn name(&self) -> &str { &self.id }

    async fn generate(&self, prompt: &str, _extra_params: Option<&serde_json::Value>) -> Result<Vec<u8>> {
        debug!(model = %self.model, "openrouter_image: generating");

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let body = serde_json::json!({
            "model":      self.model,
            "messages":   [{ "role": "user", "content": prompt }],
            "modalities": ["image"],
        });

        let resp = self.http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("X-Title", core_api::APP_NAME)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("openrouter_image: request failed: {e}"))?;

        let status = resp.status();
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| anyhow!("openrouter_image: response parse failed: {e}"))?;

        if !status.is_success() {
            let msg = json["error"]["message"].as_str().unwrap_or("unknown error");
            anyhow::bail!("openrouter_image: API error {status}: {msg}");
        }

        let data_url = json["choices"][0]["message"]["images"][0]["image_url"]["url"]
            .as_str()
            .ok_or_else(|| anyhow!("openrouter_image: no image in response — full response: {json}"))?;

        let b64 = data_url
            .strip_prefix("data:image/png;base64,")
            .or_else(|| data_url.strip_prefix("data:image/jpeg;base64,"))
            .or_else(|| data_url.strip_prefix("data:image/webp;base64,"))
            .unwrap_or(data_url);

        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| anyhow!("openrouter_image: base64 decode failed: {e}"))?;

        info!(model = %self.model, bytes = bytes.len(), "openrouter_image: generation complete");
        Ok(bytes)
    }
}
