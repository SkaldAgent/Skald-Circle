//! Ollama client (native `/api/chat` endpoint). Ported from
//! `llm-client/src/ollama.rs`. No streaming, no tool support — tool-call
//! messages are flattened to text, mirroring the previous default behavior.

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::model::{Model, ModelError, ModelRequest, ModelResponse, NamedModel, StreamDelta, Usage};

/// Ollama client. Defaults to `http://localhost:11434`. No API key required.
pub struct OllamaModel {
    base_url:      String,
    default_model: String,
    http:          reqwest::Client,
}

impl OllamaModel {
    /// `base_url` defaults to `http://localhost:11434` if `None`.
    pub fn new(base_url: Option<impl Into<String>>, default_model: impl Into<String>) -> Self {
        let url = base_url
            .map(|u| u.into())
            .unwrap_or_else(|| "http://localhost:11434".to_string());
        Self { base_url: url, default_model: default_model.into(), http: reqwest::Client::new() }
    }
}

impl NamedModel for OllamaModel {
    fn default_model(&self) -> &str { &self.default_model }
}

#[async_trait]
impl Model for OllamaModel {
    async fn complete(
        &self,
        req:    &ModelRequest,
        _deltas: Option<mpsc::Sender<StreamDelta>>,
    ) -> Result<ModelResponse, ModelError> {
        // Flatten to plain text messages: tool results and assistant
        // tool_calls are dropped (no native tool support on this path).
        let msgs: Vec<Value> = req
            .messages
            .iter()
            .filter_map(|m| {
                let role = m["role"].as_str()?;
                if !matches!(role, "system" | "user" | "assistant") {
                    return None;
                }
                let content = m["content"].as_str().unwrap_or("").to_string();
                Some(json!({ "role": role, "content": content }))
            })
            .collect();

        let mut options_obj = json!({});
        if let Some(t) = req.temperature { options_obj["temperature"] = t.into(); }
        if let Some(n) = req.max_tokens  { options_obj["num_predict"] = n.into(); }

        let body = json!({
            "model":    req.model,
            "messages": msgs,
            "stream":   false,
            "options":  options_obj,
        });

        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));

        let http_resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(ModelError::from_reqwest)?;

        let status = http_resp.status();
        if !status.is_success() {
            let resp_text = http_resp.text().await.map_err(ModelError::from_reqwest)?;
            return Err(ModelError::new(
                Some(status.as_u16()),
                format!("ollama: HTTP {status} from {url}\nbody: {resp_text}"),
            ));
        }

        let resp: Value = http_resp.json().await.map_err(ModelError::from_reqwest)?;

        let content = resp["message"]["content"]
            .as_str()
            .ok_or_else(|| ModelError::new(None, "ollama: missing content in response"))?
            .to_string();

        Ok(ModelResponse::Message {
            content,
            reasoning: None,
            usage: Usage {
                input_tokens:  resp["prompt_eval_count"].as_u64().map(|n| n as u32),
                output_tokens: resp["eval_count"].as_u64().map(|n| n as u32),
                ..Usage::default()
            },
            raw: None,
        })
    }
}
