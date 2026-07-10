use async_trait::async_trait;
use serde_json::{Value, json};

use crate::{ChatOptions, ChatResponse, ChatbotClient, Message, Role};

/// Ollama client using the native `/api/chat` endpoint.
///
/// Defaults to `http://localhost:11434`. No API key required.
pub struct OllamaClient {
    base_url: String,
    http:     reqwest::Client,
}

impl OllamaClient {
    /// `base_url` defaults to `http://localhost:11434` if `None`.
    pub fn new(base_url: Option<impl Into<String>>) -> Self {
        let url = base_url
            .map(|u| u.into())
            .unwrap_or_else(|| "http://localhost:11434".to_string());
        Self { base_url: url, http: reqwest::Client::new() }
    }
}

#[async_trait]
impl ChatbotClient for OllamaClient {
    async fn chat(
        &self,
        messages: &[Message],
        options:  &ChatOptions,
    ) -> anyhow::Result<ChatResponse> {
        let msgs: Vec<Value> = messages
            .iter()
            .map(|m| {
                let role = match m.role {
                    Role::System    => "system",
                    Role::User      => "user",
                    Role::Assistant => "assistant",
                };
                json!({ "role": role, "content": m.content })
            })
            .collect();

        let mut options_obj = json!({});
        if let Some(t) = options.temperature { options_obj["temperature"] = t.into(); }
        if let Some(n) = options.max_tokens  { options_obj["num_predict"] = n.into(); }

        let body = json!({
            "model":    options.model,
            "messages": msgs,
            "stream":   false,
            "options":  options_obj,
        });

        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));

        let resp: Value = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let content = resp["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing content in Ollama response"))?
            .to_string();

        let input_tokens  = resp["prompt_eval_count"].as_u64().map(|n| n as u32);
        let output_tokens = resp["eval_count"].as_u64().map(|n| n as u32);

        Ok(ChatResponse { content, input_tokens, output_tokens, truncated: false, reasoning_content: None, cache_read_tokens: None, cache_creation_tokens: None, cost: None })
    }
}
