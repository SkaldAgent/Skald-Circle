use std::sync::Arc;

use anyhow::{Context, Result};

use crate::chatbot::openai::OpenAiClient;
use crate::llm::{LlmModelRecord, LlmProviderRecord};
use crate::llm::providers::{RemoteLlmModelInfo, extra_with_reasoning};
use crate::provider::{ApiProvider, BuiltLlmClient, ProviderField, ProviderUiMeta, ReasoningMode, ServiceType};

/// Z.AI (Zhipu AI) — OpenAI-compatible GLM API.
///
/// Endpoint `https://api.z.ai/api/paas/v4/chat/completions`; `OpenAiClient`
/// appends `/chat/completions`, so the base URL is `.../paas/v4`.
///
/// Z.AI exposes no `GET /models` endpoint, so the model catalog is a curated
/// static list of the currently published GLM models.
pub struct ZaiProvider;

impl ZaiProvider {
    pub fn new() -> Self {
        Self
    }

    /// Base URL for the OpenAI-compatible chat endpoint (without `/chat/completions`).
    const BASE_URL: &'static str = "https://api.z.ai/api/paas/v4";

    /// Curated GLM catalog. Z.AI has no `GET /models` endpoint; this mirrors the
    /// model menu published on the Z.AI console.
    fn catalog() -> &'static [&'static str] {
        &[
            "glm-5.2",
            "glm-5.1",
            "glm-5",
            "glm-5-turbo",
            "glm-4.7",
            "glm-4.6",
            "glm-4.5",
            "glm-4-32b-0414-128k",
        ]
    }

    fn known_context_length(model_id: &str) -> Option<u64> {
        let id = model_id.to_lowercase();
        if id.contains("128k")            { Some(131_072)   }
        else if id.starts_with("glm-5")   { Some(1_048_576) } // GLM-5.x: 1M context (per Z.AI)
        else if id.starts_with("glm-4.7") { Some(200_000)   }
        else if id.starts_with("glm-4.6") { Some(200_000)   }
        else if id.starts_with("glm-4.5") { Some(131_072)   }
        else                              { None }
    }
}

#[async_trait::async_trait]
impl ApiProvider for ZaiProvider {
    fn type_id(&self) -> &'static str { "zai" }
    fn display_name(&self) -> &'static str { "Z.AI" }
    fn supported_types(&self) -> &'static [ServiceType] {
        &[ServiceType::Llm]
    }

    async fn list_llm_models(&self, _record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        let models = Self::catalog()
            .iter()
            .map(|id| RemoteLlmModelInfo {
                id:                       id.to_string(),
                name:                     id.to_string(),
                context_length:           Self::known_context_length(id),
                max_completion_tokens:    None,
                knowledge_cutoff:         None,
                capabilities:             vec!["function_calling".to_string()],
                vision:                   Some(false),
                price_input_per_million:  None,
                price_output_per_million: None,
                reasoning:                None,
            })
            .collect();

        Ok(Some(models))
    }

    fn reasoning_mode(&self, model_id: &str, _capabilities: &[String]) -> Option<ReasoningMode> {
        let id = model_id.to_lowercase();
        // GLM-5.2 (and above) additionally expose a graded `reasoning_effort`
        // on top of the thinking toggle, so offer the effort levels directly
        // ("disabled" turns thinking off).
        if id.starts_with("glm-5.2") {
            Some(ReasoningMode::ValueSet {
                values: ["disabled", "minimal", "low", "medium", "high", "xhigh", "max"]
                    .iter().map(|s| s.to_string()).collect(),
                default: Some("max".to_string()),
            })
        // Deep-thinking toggle (thinking.type) is supported by the GLM-5.x
        // series and GLM-4.5/4.6/4.7 (but not the older glm-4-32b).
        } else if id.starts_with("glm-5")
            || id.starts_with("glm-4.7")
            || id.starts_with("glm-4.6")
            || id.starts_with("glm-4.5")
        {
            Some(ReasoningMode::ValueSet {
                values:  vec!["disabled".to_string(), "enabled".to_string()],
                default: Some("enabled".to_string()),
            })
        } else {
            None
        }
    }

    fn reasoning_request(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        // "disabled" → thinking off; "enabled" → thinking on (no effort);
        // any effort level → thinking on + `reasoning_effort` (GLM-5.2+).
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
                .with_context(|| format!("provider '{}': api_key required for zai", record.name))?;
            let extra = extra_with_reasoning(self, model);
            Ok(BuiltLlmClient {
                client: Arc::new(OpenAiClient::new(Self::BASE_URL, key, extra, false)),
                prompt_cache: false,
            })
        })())
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      "zai",
            display_name: "Z.AI",
            description:  Some("Zhipu AI GLM models (OpenAI-compatible)"),
            color:        "#4f46e5",
            icon:         "bi-stars",
            fields: &[
                ProviderField { key: "api_key", label: "API Key", required: true, secret: true },
            ],
        }
    }
}
