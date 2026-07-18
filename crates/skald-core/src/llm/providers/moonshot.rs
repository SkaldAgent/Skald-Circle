use anyhow::{Result, anyhow};

use crate::llm::{LlmModelRecord, LlmProviderRecord};
use crate::llm::providers::{RemoteLlmModelInfo, build_openai_llm, fetch_openai_models};
use crate::provider::{ApiProvider, BuiltLlmClient, ProviderField, ProviderUiMeta, ReasoningMode, ServiceType};

/// Moonshot AI — pay-as-you-go platform.
///
/// Endpoint `https://api.moonshot.ai/v1/chat/completions` (OpenAI-compatible);
/// `OpenAiClient` appends `/chat/completions`, so the base URL is `.../v1`.
///
/// `GET /models` returns the full catalog including `context_length`,
/// `supports_image_in` and `supports_reasoning`, so the model list is entirely
/// endpoint-driven. Thinking models (kimi-k2-thinking…) always reason — the
/// platform exposes no request-level knob, hence no `ReasoningMode`.
pub struct MoonshotProvider {
    http: reqwest::Client,
}

/// Moonshot AI — Kimi Code subscription.
///
/// Endpoint `https://api.kimi.com/coding/v1/chat/completions` (OpenAI-compatible).
/// The model catalog comes from `GET /models` like the platform; only the
/// metadata the endpoint omits (context size, vision) is filled from the
/// published docs. K3 is the only model with a reasoning knob: a graded
/// `reasoning_effort` (`low`/`high`/`max`, default `max`); the kimi-for-coding
/// series (K2.7 Code) always thinks and has no toggle.
pub struct MoonshotCodeProvider {
    http: reqwest::Client,
}

impl MoonshotProvider {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }

    const BASE_URL: &'static str = "https://api.moonshot.ai/v1";
}

impl MoonshotCodeProvider {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }

    const BASE_URL: &'static str = "https://api.kimi.com/coding/v1";

    /// Fills the metadata the Kimi Code `/models` endpoint may omit, from the
    /// published docs: K3 → up to 1M context + native visual understanding;
    /// the kimi-for-coding series → 256k. Values already present in the
    /// endpoint response (e.g. a tier-specific context size) always win.
    fn enrich(info: &mut RemoteLlmModelInfo) {
        let id = info.id.to_lowercase();
        if info.context_length.is_none() {
            if id.starts_with("k3") {
                info.context_length = Some(1_048_576);
            } else if id.starts_with("kimi-for-coding") {
                info.context_length = Some(262_144);
            }
        }
        if info.vision.is_none() && id.starts_with("k3") {
            info.vision = Some(true);
        }
    }
}

/// Shared model listing for both Moonshot APIs: same envelope, same per-model
/// fields (`context_length`, `supports_image_in`, `supports_reasoning` — all
/// optional, absent fields are left for the caller to enrich).
async fn list_models(
    http:     &reqwest::Client,
    base_url: &str,
    record:   &LlmProviderRecord,
    who:      &str,
) -> Result<Vec<RemoteLlmModelInfo>> {
    let api_key = record.api_key.as_deref()
        .ok_or_else(|| anyhow!("provider '{}': api_key required for {who} model listing", record.name))?;

    let raw = fetch_openai_models(http, base_url, Some(api_key), who).await?;
    let models = raw
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            let mut capabilities = vec!["function_calling".to_string()];
            if m["supports_reasoning"].as_bool().unwrap_or(false) {
                capabilities.push("reasoning".to_string());
            }
            if m["supports_image_in"].as_bool().unwrap_or(false) {
                capabilities.push("vision".to_string());
            }
            Some(RemoteLlmModelInfo {
                id,
                name:                     m["id"].as_str()?.to_string(),
                context_length:           m["context_length"].as_u64(),
                max_completion_tokens:    None,
                knowledge_cutoff:         None,
                capabilities,
                vision:                   m["supports_image_in"].as_bool(),
                price_input_per_million:  None,
                price_output_per_million: None,
                reasoning:                None,
            })
        })
        .collect();

    Ok(models)
}

#[async_trait::async_trait]
impl ApiProvider for MoonshotProvider {
    fn type_id(&self) -> &'static str { "moonshot" }
    fn display_name(&self) -> &'static str { "Moonshot AI pay-as-you-go" }
    fn supported_types(&self) -> &'static [ServiceType] {
        &[ServiceType::Llm]
    }

    async fn list_llm_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        Ok(Some(list_models(&self.http, Self::BASE_URL, record, "Moonshot AI").await?))
    }

    fn build_llm(&self, record: &LlmProviderRecord, model: &LlmModelRecord) -> Option<Result<BuiltLlmClient>> {
        Some(build_openai_llm(self, Self::BASE_URL, record, model, false))
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      "moonshot",
            display_name: "Moonshot AI pay-as-you-go",
            description:  Some("Kimi models on the Moonshot AI platform (OpenAI-compatible)"),
            color:        "#2563eb",
            icon:         "bi-moon-stars",
            lists_models: true,
            fields: &[
                ProviderField { key: "api_key", label: "API Key", required: true, secret: true },
            ],
        }
    }
}

#[async_trait::async_trait]
impl ApiProvider for MoonshotCodeProvider {
    fn type_id(&self) -> &'static str { "moonshot_code" }
    fn display_name(&self) -> &'static str { "Moonshot AI Kimi Code" }
    fn supported_types(&self) -> &'static [ServiceType] {
        &[ServiceType::Llm]
    }

    async fn list_llm_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        let mut models = list_models(&self.http, Self::BASE_URL, record, "Kimi Code").await?;
        for m in &mut models {
            Self::enrich(m);
        }
        Ok(Some(models))
    }

    fn reasoning_mode(&self, model_id: &str, _capabilities: &[String]) -> Option<ReasoningMode> {
        let id = model_id.to_lowercase();
        // K3 exposes a graded `reasoning_effort` (low/high/max; default max).
        // "disabled" turns thinking off — the API then routes to K2.6.
        // kimi-for-coding (K2.7 Code) always thinks and has no knob.
        if id.starts_with("k3") {
            Some(ReasoningMode::ValueSet {
                values: ["disabled", "low", "high", "max"]
                    .iter().map(|s| s.to_string()).collect(),
                default: Some("max".to_string()),
            })
        } else {
            None
        }
    }

    fn reasoning_request(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        // The Kimi Code API accepts a flat `reasoning_effort`: "none" disables
        // thinking, low/high/max select the effort (unknown values → HTTP 400).
        match value.as_str()? {
            "disabled" => Some(serde_json::json!({ "reasoning_effort": "none" })),
            effort     => Some(serde_json::json!({ "reasoning_effort": effort })),
        }
    }

    fn build_llm(&self, record: &LlmProviderRecord, model: &LlmModelRecord) -> Option<Result<BuiltLlmClient>> {
        Some(build_openai_llm(self, Self::BASE_URL, record, model, false))
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      "moonshot_code",
            display_name: "Moonshot AI Kimi Code",
            description:  Some("Kimi Code subscription models — k3 / kimi-for-coding (OpenAI-compatible)"),
            color:        "#000000",
            icon:         "bi-code-slash",
            lists_models: true,
            fields: &[
                ProviderField { key: "api_key", label: "API Key", required: true, secret: true },
            ],
        }
    }
}
