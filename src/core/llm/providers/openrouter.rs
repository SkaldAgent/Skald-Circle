use std::sync::Arc;

use anyhow::{Context, Result, anyhow};

use crate::core::chatbot::openai::OpenAiClient;
use crate::core::image_generate::ImageGenerateModelRecord;
use crate::core::image_generate::openrouter_image::OpenRouterImageGenerator;
use crate::core::llm::{LlmModelRecord, LlmProviderRecord};
use crate::core::llm::providers::{RemoteLlmModelInfo, extra_with_reasoning};
use crate::core::transcribe::TranscribeModelRecord;
use crate::core::transcribe::openai_audio::OpenAiAudioTranscriber;
use crate::core::tts::TtsModelRecord;
use crate::core::tts::openai_tts::OpenAiTtsSynthesiser;
use crate::core::provider::{ApiProvider, BuiltLlmClient, ProviderField, ProviderUiMeta, ReasoningMode, ServiceType};

pub struct OpenRouterProvider {
    http: reqwest::Client,
}

impl OpenRouterProvider {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }

    /// Builds a `ReasoningMode` from OpenRouter's per-model `reasoning` object
    /// (`/api/v1/models`): `supported_efforts` → discrete effort levels,
    /// otherwise `supports_max_tokens` → a token-budget range. Non-reasoning
    /// models omit the object entirely → `None`. The UI's "— off —" option
    /// (a null value → no reasoning param sent) covers disabling.
    fn parse_reasoning(v: &serde_json::Value) -> Option<ReasoningMode> {
        if !v.is_object() {
            return None;
        }
        let default = v["default_effort"].as_str().map(String::from);
        let efforts: Vec<String> = v["supported_efforts"].as_array()
            .map(|a| a.iter().filter_map(|e| e.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if !efforts.is_empty() {
            Some(ReasoningMode::ValueSet { values: efforts, default })
        } else if v["supports_max_tokens"].as_bool().unwrap_or(false) {
            Some(ReasoningMode::Range {
                min: 1024, max: 32_000, step: Some(1024), default: Some(8192),
                unit: Some("tokens".to_string()),
            })
        } else {
            None
        }
    }

    async fn fetch_catalog(&self, api_key: &str) -> Result<Vec<RemoteLlmModelInfo>> {
        let resp: serde_json::Value = self.http
            .get("https://openrouter.ai/api/v1/models")
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(|e| anyhow!("OpenRouter request failed: {e}"))?
            .json()
            .await
            .map_err(|e| anyhow!("OpenRouter response parse failed: {e}"))?;

        let models = resp["data"]
            .as_array()
            .ok_or_else(|| anyhow!("unexpected OpenRouter response shape"))?
            .iter()
            .filter_map(|m| {
                let id   = m["id"].as_str()?.to_string();
                let name = m["name"].as_str().unwrap_or(&id).to_string();
                let context_length = m["context_length"].as_u64();
                let price_input  = m["pricing"]["prompt"].as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|v| v * 1_000_000.0);
                let price_output = m["pricing"]["completion"].as_str()
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|v| v * 1_000_000.0);
                let capabilities = {
                    let mut caps = vec!["function_calling".to_string()];
                    if let Some(params) = m["supported_parameters"].as_array() {
                        for p in params {
                            if let Some(s) = p.as_str() {
                                match s {
                                    "tools" => caps.push("function_calling".to_string()),
                                    "vision" | "image" => caps.push("vision".to_string()),
                                    "stream" => caps.push("streaming".to_string()),
                                    "reasoning" | "reasoning_effort" => caps.push("reasoning".to_string()),
                                    _ => {}
                                }
                            }
                        }
                    }
                    caps.sort();
                    caps.dedup();
                    caps
                };
                let vision = Some(capabilities.contains(&"vision".to_string()));
                let reasoning = Self::parse_reasoning(&m["reasoning"]);
                Some(RemoteLlmModelInfo {
                    id, name, context_length,
                    max_completion_tokens:    None,
                    knowledge_cutoff:         None,
                    capabilities,
                    vision,
                    price_input_per_million:  price_input,
                    price_output_per_million: price_output,
                    reasoning,
                })
            })
            .collect();

        Ok(models)
    }
}

#[async_trait::async_trait]
impl ApiProvider for OpenRouterProvider {
    fn type_id(&self) -> &'static str { "openrouter" }
    fn display_name(&self) -> &'static str { "OpenRouter" }
    fn supported_types(&self) -> &'static [ServiceType] {
        &[ServiceType::Llm, ServiceType::Transcribe, ServiceType::ImageGenerate, ServiceType::Tts]
    }

    async fn list_llm_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        let api_key = record.api_key.as_deref()
            .ok_or_else(|| anyhow!("provider '{}': api_key required for openrouter model listing", record.name))?;
        Ok(Some(self.fetch_catalog(api_key).await?))
    }

    fn reasoning_mode(&self, _model_id: &str, capabilities: &[String]) -> Option<ReasoningMode> {
        // Fallback for stored/manually-added models with no catalog descriptor.
        // The precise per-model set comes from `parse_reasoning` in the catalog;
        // here we offer OpenRouter's full accepted effort set.
        if capabilities.iter().any(|c| c == "reasoning") {
            Some(ReasoningMode::ValueSet {
                values: ["minimal", "low", "medium", "high", "xhigh", "max"]
                    .iter().map(|s| s.to_string()).collect(),
                default: Some("medium".to_string()),
            })
        } else {
            None
        }
    }

    fn reasoning_request(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        // String → effort level; number → token budget (max_tokens).
        if let Some(effort) = value.as_str() {
            Some(serde_json::json!({ "reasoning": { "effort": effort } }))
        } else {
            let budget = value.as_i64().filter(|n| *n > 0)?;
            Some(serde_json::json!({ "reasoning": { "max_tokens": budget } }))
        }
    }

    fn build_llm(&self, record: &LlmProviderRecord, model: &LlmModelRecord) -> Option<Result<BuiltLlmClient>> {
        Some((|| {
            let key = record.api_key.as_deref()
                .with_context(|| format!("provider '{}': api_key required for openrouter", record.name))?;
            // Anthropic prompt-caching only works for models served by Anthropic on OpenRouter.
            let prompt_cache = model.model_id.starts_with("anthropic/");
            let extra = extra_with_reasoning(self, model);
            Ok(BuiltLlmClient {
                client: Arc::new(OpenAiClient::new("https://openrouter.ai/api/v1", key, extra, prompt_cache)),
                prompt_cache,
            })
        })())
    }

    fn build_tts(&self, record: &LlmProviderRecord, model: &TtsModelRecord) -> Option<Result<Arc<dyn crate::core::tts::TextToSpeech>>> {
        Some((|| {
            let base_url = record.base_url.clone()
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());
            let api_key = record.api_key.clone()
                .with_context(|| format!("provider '{}': api_key required for openrouter", record.name))?;
            Ok(Arc::new(OpenAiTtsSynthesiser::new(
                &model.name, base_url, api_key, &model.model_id,
                model.voice_id.clone(), model.instructions.clone(), model.response_format.clone(),
            )) as Arc<dyn crate::core::tts::TextToSpeech>)
        })())
    }

    fn build_transcriber(&self, record: &LlmProviderRecord, model: &TranscribeModelRecord) -> Option<Result<Arc<dyn crate::core::transcribe::Transcribe>>> {
        Some((|| {
            let base_url = record.base_url.clone()
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());
            let api_key = record.api_key.clone()
                .with_context(|| format!("provider '{}': api_key required for openrouter", record.name))?;
            Ok(Arc::new(OpenAiAudioTranscriber::new(
                &model.name, base_url, api_key, &model.model_id, model.language.clone(),
            )) as Arc<dyn crate::core::transcribe::Transcribe>)
        })())
    }

    fn build_image_generator(&self, record: &LlmProviderRecord, model: &ImageGenerateModelRecord) -> Option<Result<Arc<dyn crate::core::image_generate::ImageGenerate>>> {
        Some((|| {
            let base_url = record.base_url.clone()
                .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_string());
            let api_key = record.api_key.clone()
                .with_context(|| format!("provider '{}': api_key required for openrouter", record.name))?;
            Ok(Arc::new(OpenRouterImageGenerator::new(
                &model.name, base_url, api_key, &model.model_id,
            )) as Arc<dyn crate::core::image_generate::ImageGenerate>)
        })())
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      "openrouter",
            display_name: "OpenRouter",
            description:  None,
            color:        "#8b5cf6",
            icon:         "bi-hdd-stack",
            fields: &[
                ProviderField { key: "api_key", label: "API Key", required: true, secret: true },
            ],
        }
    }
}
