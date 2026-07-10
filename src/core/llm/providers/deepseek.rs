use std::sync::Arc;

use anyhow::{Context, Result, anyhow};

use crate::core::chatbot::openai::OpenAiClient;
use crate::core::llm::{LlmModelRecord, LlmProviderRecord};
use crate::core::llm::providers::{RemoteLlmModelInfo, extra_with_reasoning};
use crate::core::provider::{ApiProvider, BuiltLlmClient, ProviderField, ProviderUiMeta, ReasoningMode, ServiceType};

pub struct DeepSeekProvider {
    http: reqwest::Client,
}

impl DeepSeekProvider {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }

    fn known_context_length(model_id: &str) -> Option<u64> {
        let id = model_id.to_lowercase();
        if id.contains("coder")                                     { Some(16384)     }
        else if id.contains("reasoner")                             { Some(65536)     }
        else if id.starts_with("deepseek-v4")                       { Some(1_048_576) }
        else if id.starts_with("deepseek-chat") || id.starts_with("deepseek-v3") { Some(65536) }
        else                                                        { None }
    }

    fn known_max_output(model_id: &str) -> Option<u64> {
        if model_id.to_lowercase().starts_with("deepseek-v4") { Some(393_216) } else { None }
    }

    fn known_capabilities(model_id: &str) -> Vec<String> {
        let mut caps = vec!["function_calling".to_string()];
        if model_id.to_lowercase().contains("reasoner") {
            caps.push("reasoning".to_string());
        }
        caps
    }
}

#[async_trait::async_trait]
impl ApiProvider for DeepSeekProvider {
    fn type_id(&self) -> &'static str { "deepseek" }
    fn display_name(&self) -> &'static str { "DeepSeek" }
    fn supported_types(&self) -> &'static [ServiceType] {
        &[ServiceType::Llm]
    }

    async fn list_llm_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        let api_key = record.api_key.as_deref()
            .ok_or_else(|| anyhow!("provider '{}': api_key required for deepseek model listing", record.name))?;

        let resp: serde_json::Value = self.http
            .get("https://api.deepseek.com/models")
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(|e| anyhow!("DeepSeek request failed: {e}"))?
            .error_for_status()
            .map_err(|e| anyhow!("DeepSeek error response: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("DeepSeek response parse failed: {e}"))?;

        let models = resp["data"]
            .as_array()
            .ok_or_else(|| anyhow!("unexpected DeepSeek response shape"))?
            .iter()
            .filter_map(|m| {
                let id   = m["id"].as_str()?.to_string();
                let name = id.clone();
                let context_length = Self::known_context_length(&id).or_else(|| m["context_length"].as_u64());
                let capabilities   = Self::known_capabilities(&id);
                let max_output     = Self::known_max_output(&id);
                Some(RemoteLlmModelInfo {
                    id, name, context_length,
                    max_completion_tokens:    max_output,
                    knowledge_cutoff:         None,
                    capabilities,
                    vision:                   None,
                    price_input_per_million:  None,
                    price_output_per_million: None,
                    reasoning:                None,
                })
            })
            .collect();

        Ok(Some(models))
    }

    fn reasoning_mode(&self, model_id: &str, capabilities: &[String]) -> Option<ReasoningMode> {
        // Thinking mode (thinking.type) + graded reasoning_effort. "disabled"
        // turns thinking off; effort levels low/medium map to high, xhigh to max.
        let id = model_id.to_lowercase();
        if capabilities.iter().any(|c| c == "reasoning")
            || id.contains("reasoner")
            || id.starts_with("deepseek-v4")
        {
            Some(ReasoningMode::ValueSet {
                values: ["disabled", "low", "medium", "high", "xhigh", "max"]
                    .iter().map(|s| s.to_string()).collect(),
                default: Some("high".to_string()),
            })
        } else {
            None
        }
    }

    fn reasoning_request(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        // "disabled" → thinking off; "enabled" → thinking on (no effort);
        // any effort level → thinking on + `reasoning_effort`.
        match value.as_str()? {
            "disabled" => Some(serde_json::json!({ "thinking": { "type": "disabled" } })),
            "enabled"  => Some(serde_json::json!({ "thinking": { "type": "enabled" } })),
            effort     => Some(serde_json::json!({
                "thinking":         { "type": "enabled" },
                "reasoning_effort": effort,
            })),
        }
    }

    fn build_llm(&self, record: &LlmProviderRecord, model: &LlmModelRecord) -> Option<Result<BuiltLlmClient>> {
        Some((|| {
            let key = record.api_key.as_deref()
                .with_context(|| format!("provider '{}': api_key required for deepseek", record.name))?;
            let extra = extra_with_reasoning(self, model);
            Ok(BuiltLlmClient {
                client: Arc::new(OpenAiClient::new("https://api.deepseek.com/v1", key, extra, false)),
                prompt_cache: false,
            })
        })())
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      "deepseek",
            display_name: "DeepSeek",
            description:  None,
            color:        "#0ea5e9",
            icon:         "bi-search",
            fields: &[
                ProviderField { key: "api_key", label: "API Key", required: true, secret: true },
            ],
        }
    }
}
