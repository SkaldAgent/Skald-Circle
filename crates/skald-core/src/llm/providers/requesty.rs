use anyhow::{Result, anyhow};

use crate::llm::providers::{RemoteLlmModelInfo, build_openai_llm, fetch_openai_models};
use crate::llm::{LlmModelRecord, LlmProviderRecord};
use crate::provider::{ApiProvider, BuiltLlmClient, ProviderField, ProviderUiMeta, ReasoningMode, ServiceType};

/// Requesty router base URL.
const BASE_URL: &str = "https://router.requesty.ai/v1";

/// Reasoning effort values Requesty passes through to supporting models
/// (OpenAI o-series, Anthropic extended thinking, DeepSeek-R, etc.).
const REASONING_EFFORTS: &[&str] = &["minimal", "low", "medium", "high"];

pub struct RequestyProvider {
    /// Lazy: a `reqwest::Client` can only be built after the process installs
    /// a crypto provider (done by the shell at startup), so we defer until
    /// first use — same pattern as `DeclaredProvider`.
    http: std::sync::OnceLock<reqwest::Client>,
}

impl RequestyProvider {
    pub fn new() -> Self {
        Self { http: std::sync::OnceLock::new() }
    }

    fn http(&self) -> &reqwest::Client {
        self.http.get_or_init(reqwest::Client::new)
    }

    async fn fetch_catalog(&self, api_key: &str) -> Result<Vec<RemoteLlmModelInfo>> {
        let raw = fetch_openai_models(self.http(), BASE_URL, Some(api_key), "Requesty").await?;
        Ok(raw.iter().filter_map(map_model).collect())
    }
}

/// Maps one raw JSON model object from Requesty's `GET /v1/models` response
/// to `RemoteLlmModelInfo`. The endpoint returns flat fields (not nested
/// objects): `context_window`, `max_output_tokens`, `input_price`,
/// `output_price` (all per-token USD), and `supports_*` booleans.
fn map_model(m: &serde_json::Value) -> Option<RemoteLlmModelInfo> {
    let id = m["id"].as_str()?.to_string();

    let context_length        = m["context_window"].as_u64();
    let max_completion_tokens = m["max_output_tokens"].as_u64();

    // Prices are per-token USD → convert to per-million.
    let price_input_per_million  = m["input_price"].as_f64().map(|v| v * 1_000_000.0);
    let price_output_per_million = m["output_price"].as_f64().map(|v| v * 1_000_000.0);

    let vision    = m["supports_vision"].as_bool().unwrap_or(false);
    let reasoning = m["supports_reasoning"].as_bool().unwrap_or(false);

    let mut capabilities = vec!["function_calling".to_string()];
    if vision    { capabilities.push("vision".to_string()); }
    if reasoning { capabilities.push("reasoning".to_string()); }
    capabilities.sort();
    capabilities.dedup();

    Some(RemoteLlmModelInfo {
        name: id.clone(),
        id,
        context_length,
        max_completion_tokens,
        knowledge_cutoff:         None,
        capabilities,
        vision: Some(vision),
        price_input_per_million,
        price_output_per_million,
        reasoning: None,
    })
}

#[async_trait::async_trait]
impl ApiProvider for RequestyProvider {
    fn type_id(&self) -> &'static str { "requesty" }
    fn display_name(&self) -> &'static str { "Requesty" }
    fn supported_types(&self) -> &'static [ServiceType] {
        &[ServiceType::Llm]
    }

    async fn list_llm_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        let api_key = record.api_key.as_deref()
            .ok_or_else(|| anyhow!("provider '{}': api_key required for requesty model listing", record.name))?;
        Ok(Some(self.fetch_catalog(api_key).await?))
    }

    fn reasoning_mode(&self, _model_id: &str, capabilities: &[String]) -> Option<ReasoningMode> {
        if capabilities.iter().any(|c| c == "reasoning") {
            Some(ReasoningMode::ValueSet {
                values:  REASONING_EFFORTS.iter().map(|s| s.to_string()).collect(),
                default: Some("medium".to_string()),
            })
        } else {
            None
        }
    }

    fn reasoning_request(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        value.as_str().map(|s| serde_json::json!({ "reasoning_effort": s }))
    }

    fn build_llm(&self, record: &LlmProviderRecord, model: &LlmModelRecord) -> Option<Result<BuiltLlmClient>> {
        Some(build_openai_llm(self, BASE_URL, record, model, false))
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      "requesty",
            display_name: "Requesty",
            description:  Some("Requesty AI gateway — 300+ models from OpenAI, Anthropic, Google and more"),
            color:        "#10b981",
            icon:         "bi-shuffle",
            lists_models: true,
            fields: &[
                ProviderField { key: "api_key", label: "API Key", required: true, secret: true },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_full_model() {
        let m = serde_json::json!({
            "id": "anthropic/claude-opus-4-8",
            "object": "model",
            "created": 1715367049,
            "owned_by": "anthropic",
            "context_window": 1048576,
            "max_output_tokens": 128000,
            "input_price": 0.000015,
            "output_price": 0.000075,
            "supports_vision": true,
            "supports_reasoning": true
        });
        let info = map_model(&m).unwrap();
        assert_eq!(info.id, "anthropic/claude-opus-4-8");
        assert_eq!(info.context_length, Some(1_048_576));
        assert_eq!(info.max_completion_tokens, Some(128_000));
        assert_eq!(info.price_input_per_million, Some(15.0));
        assert_eq!(info.price_output_per_million, Some(75.0));
        assert_eq!(info.vision, Some(true));
        assert!(info.capabilities.contains(&"vision".to_string()));
        assert!(info.capabilities.contains(&"reasoning".to_string()));
        assert!(info.capabilities.contains(&"function_calling".to_string()));
    }

    #[test]
    fn map_bare_model() {
        // Some models may omit the enriched fields entirely.
        let m = serde_json::json!({
            "id": "experimental/model-x",
            "object": "model",
            "owned_by": "test"
        });
        let info = map_model(&m).unwrap();
        assert_eq!(info.id, "experimental/model-x");
        assert_eq!(info.context_length, None);
        assert_eq!(info.max_completion_tokens, None);
        assert_eq!(info.price_input_per_million, None);
        assert_eq!(info.vision, Some(false));
        assert!(!info.capabilities.contains(&"vision".to_string()));
    }

    #[test]
    fn map_non_vision_non_reasoning() {
        let m = serde_json::json!({
            "id": "deepseek/deepseek-chat",
            "context_window": 64000,
            "supports_vision": false,
            "supports_reasoning": false
        });
        let info = map_model(&m).unwrap();
        assert_eq!(info.context_length, Some(64_000));
        assert_eq!(info.vision, Some(false));
        assert!(!info.capabilities.contains(&"reasoning".to_string()));
    }

    #[test]
    fn reasoning_request_maps_effort() {
        let p = RequestyProvider::new();
        let req = |v: &str| p.reasoning_request(&serde_json::json!(v)).unwrap();
        assert_eq!(req("high"),    serde_json::json!({ "reasoning_effort": "high" }));
        assert_eq!(req("minimal"), serde_json::json!({ "reasoning_effort": "minimal" }));
        assert!(p.reasoning_request(&serde_json::json!(42)).is_none());
    }

    #[test]
    fn reasoning_mode_from_capabilities() {
        let p = RequestyProvider::new();
        assert!(p.reasoning_mode("any", &["reasoning".to_string()]).is_some());
        assert!(p.reasoning_mode("any", &[]).is_none());
    }
}
