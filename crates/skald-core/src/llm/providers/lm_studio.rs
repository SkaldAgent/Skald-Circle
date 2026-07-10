use std::sync::Arc;

use anyhow::{Result, anyhow};

use crate::chatbot::lm_studio::LmStudioClient;
use crate::llm::{LlmModelRecord, LlmProviderRecord};
use crate::llm::providers::RemoteLlmModelInfo;
use crate::provider::{ApiProvider, BuiltLlmClient, ProviderField, ProviderUiMeta, ServiceType};

pub struct LmStudioProvider {
    http: reqwest::Client,
}

impl LmStudioProvider {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }

    fn base_url(record: &LlmProviderRecord) -> String {
        record.base_url.clone()
            .unwrap_or_else(|| "http://localhost:1234/v1".to_string())
    }
}

#[async_trait::async_trait]
impl ApiProvider for LmStudioProvider {
    fn type_id(&self) -> &'static str { "lm_studio" }
    fn display_name(&self) -> &'static str { "LM Studio" }
    fn supported_types(&self) -> &'static [ServiceType] {
        &[ServiceType::Llm]
    }

    async fn list_llm_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        let url = format!("{}/models", Self::base_url(record).trim_end_matches('/'));
        let resp: serde_json::Value = self.http
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("LM Studio request failed: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("LM Studio response parse failed: {e}"))?;

        let models = resp["data"]
            .as_array()
            .ok_or_else(|| anyhow!("unexpected LM Studio response shape"))?
            .iter()
            .filter_map(|m| {
                let id = m["id"].as_str()?.to_string();
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

    fn build_llm(&self, record: &LlmProviderRecord, _model: &LlmModelRecord) -> Option<Result<BuiltLlmClient>> {
        Some(Ok(BuiltLlmClient {
            client: Arc::new(LmStudioClient::new(record.base_url.as_deref())),
            prompt_cache: false,
        }))
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      "lm_studio",
            display_name: "LM Studio",
            description:  Some("Local models via LM Studio"),
            color:        "#6b7280",
            icon:         "bi-window-stack",
            fields: &[
                ProviderField { key: "base_url", label: "Base URL", required: false, secret: false },
            ],
        }
    }
}
