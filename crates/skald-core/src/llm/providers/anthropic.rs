use std::sync::Arc;

use anyhow::{Context, Result, anyhow};

use crate::chatbot::anthropic::AnthropicClient;
use crate::llm::{LlmModelRecord, LlmProviderRecord};
use crate::llm::providers::{RemoteLlmModelInfo, extra_with_reasoning};
use crate::provider::{ApiProvider, BuiltLlmClient, ProviderField, ProviderUiMeta, ReasoningMode, ServiceType};

pub struct AnthropicProvider {
    http: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }
}

#[async_trait::async_trait]
impl ApiProvider for AnthropicProvider {
    fn type_id(&self) -> &'static str { "anthropic" }
    fn display_name(&self) -> &'static str { "Anthropic" }
    fn supported_types(&self) -> &'static [ServiceType] {
        &[ServiceType::Llm]
    }

    fn dtl_format(&self) -> Option<&str> {
        // Every Anthropic model that opts in (via the `tool_search` capability)
        // uses the custom client-side tool_reference format.
        Some("anthropic_tool_reference")
    }

    async fn list_llm_models(&self, _record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        Ok(None)
    }

    async fn llm_model_info(&self, record: &LlmProviderRecord, model_id: &str) -> Result<Option<RemoteLlmModelInfo>> {
        let api_key = record.api_key.as_deref()
            .ok_or_else(|| anyhow!("provider '{}': api_key required for anthropic model_info", record.name))?;

        let url = format!("https://api.anthropic.com/v1/models/{model_id}");
        let resp: serde_json::Value = self.http
            .get(&url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| anyhow!("Anthropic model_info request failed: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("Anthropic model_info response parse failed: {e}"))?;

        let id   = resp["id"].as_str().ok_or_else(|| anyhow!("missing 'id' in Anthropic response"))?.to_string();
        let name = resp["display_name"].as_str().unwrap_or(&id).to_string();

        Ok(Some(RemoteLlmModelInfo {
            id,
            name,
            context_length:           resp["context_window"].as_u64(),
            max_completion_tokens:    resp["max_output_tokens"].as_u64(),
            knowledge_cutoff:         None,
            capabilities:             vec![],
            vision:                   None,
            price_input_per_million:  None,
            price_output_per_million: None,
            reasoning:                None,
        }))
    }

    fn reasoning_mode(&self, model_id: &str, capabilities: &[String]) -> Option<ReasoningMode> {
        // Extended thinking → numeric token budget. Available on Claude 3.7 and
        // the 4.x/5.x families (not the 3.5/3-opus generation).
        let id = model_id.to_lowercase();
        let supports = capabilities.iter().any(|c| c == "reasoning")
            || id.contains("3-7")
            || id.contains("-4") || id.contains("-5")
            || id.contains("opus-4") || id.contains("sonnet-4") || id.contains("haiku-4");
        if supports {
            Some(ReasoningMode::Range {
                min:     1024,
                max:     32_000,
                step:    Some(1024),
                default: Some(8192),
                unit:    Some("tokens".to_string()),
            })
        } else {
            None
        }
    }

    fn reasoning_request(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        // value is a JSON number (budget_tokens).
        let budget = value.as_i64().filter(|n| *n > 0)?;
        Some(serde_json::json!({
            "thinking": { "type": "enabled", "budget_tokens": budget }
        }))
    }

    fn build_llm(&self, record: &LlmProviderRecord, model: &LlmModelRecord) -> Option<Result<BuiltLlmClient>> {
        Some((|| {
            let key = record.api_key.as_deref()
                .with_context(|| format!("provider '{}': api_key required for anthropic", record.name))?;
            // Merge model extra_params + reasoning (thinking) into the request body.
            let extra = extra_with_reasoning(self, model);
            // Prompt caching is enabled exactly when this model runs dynamic tool
            // loading (the `tool_search` capability → custom tool_reference): the
            // deferred toolset keeps the tools prefix stable and the message builder
            // tags the static system block with cache_control, which the client
            // renders into the `system` array. Without DTL the native Anthropic path
            // stays uncached, as before.
            let prompt_cache = model.capabilities.iter().any(|c| c == "tool_search");
            Ok(BuiltLlmClient {
                client: Arc::new(AnthropicClient::with_extra_body(key, extra)),
                prompt_cache,
            })
        })())
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      "anthropic",
            display_name: "Anthropic",
            description:  None,
            color:        "#d4a574",
            icon:         "bi-chat-square-dots",
            lists_models: false,
            fields: &[
                ProviderField { key: "api_key", label: "API Key", required: true, secret: true },
            ],
        }
    }
}
