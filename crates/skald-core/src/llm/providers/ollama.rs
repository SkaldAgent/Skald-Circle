use std::sync::Arc;

use anyhow::{Result, anyhow};

use agent_loop::models::OllamaModel;
use crate::llm::{LlmModelRecord, LlmProviderRecord};
use crate::llm::providers::RemoteLlmModelInfo;
use crate::provider::{ApiProvider, BuiltLlmClient, ProviderField, ProviderUiMeta, ServiceType};

pub struct OllamaProvider {
    http: reqwest::Client,
}

impl OllamaProvider {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }

    fn base_url(record: &LlmProviderRecord) -> String {
        record.base_url.clone()
            .unwrap_or_else(|| "http://localhost:11434".to_string())
    }

    fn parse_model_info(show: &serde_json::Value, model_id: &str) -> RemoteLlmModelInfo {
        let context_length = show["model_info"]["llm.context_length"]
            .as_u64()
            .or_else(|| {
                show["model_info"]["llm.context_length"]
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
            });
        RemoteLlmModelInfo {
            name:                     model_id.to_string(),
            id:                       model_id.to_string(),
            context_length,
            max_completion_tokens:    None,
            knowledge_cutoff:         None,
            capabilities:             vec![],
            vision:                   None,
            price_input_per_million:  None,
            price_output_per_million: None,
            reasoning:                None,
        }
    }
}

#[async_trait::async_trait]
impl ApiProvider for OllamaProvider {
    fn type_id(&self) -> &'static str { "ollama" }
    fn display_name(&self) -> &'static str { "Ollama" }
    fn supported_types(&self) -> &'static [ServiceType] {
        &[ServiceType::Llm]
    }

    async fn list_llm_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        let url = format!("{}/api/tags", Self::base_url(record).trim_end_matches('/'));
        let resp: serde_json::Value = self.http
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("Ollama request failed: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("Ollama response parse failed: {e}"))?;

        let models = resp["models"]
            .as_array()
            .ok_or_else(|| anyhow!("unexpected Ollama response shape"))?
            .iter()
            .filter_map(|m| {
                let id = m["name"].as_str()?.to_string();
                Some(RemoteLlmModelInfo {
                    name: id.clone(), id,
                    context_length:           None,
                    max_completion_tokens:    None,
                    knowledge_cutoff:         None,
                    capabilities:             vec![],
                    vision:                   None,
                    price_input_per_million:  None,
                    price_output_per_million: None,
                    reasoning:                None,
                })
            })
            .collect();

        Ok(Some(models))
    }

    async fn llm_model_info(&self, record: &LlmProviderRecord, model_id: &str) -> Result<Option<RemoteLlmModelInfo>> {
        let url  = format!("{}/api/show", Self::base_url(record).trim_end_matches('/'));
        let body = serde_json::json!({ "name": model_id });
        let resp: serde_json::Value = self.http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("Ollama model_info request failed: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("Ollama model_info response parse failed: {e}"))?;
        Ok(Some(Self::parse_model_info(&resp, model_id)))
    }

    fn build_llm(&self, record: &LlmProviderRecord, model: &LlmModelRecord) -> Option<Result<BuiltLlmClient>> {
        Some(Ok(BuiltLlmClient {
            client: Arc::new(OllamaModel::new(record.base_url.as_deref(), model.model_id.clone())),
            prompt_cache: false,
        }))
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      "ollama",
            display_name: "Ollama",
            description:  Some("Local models via Ollama"),
            color:        "#f97316",
            icon:         "bi-terminal",
            lists_models: true,
            fields: &[
                ProviderField { key: "base_url", label: "Base URL", required: false, secret: false },
            ],
        }
    }
}
