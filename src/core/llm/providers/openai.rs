use std::sync::Arc;

use anyhow::{Context, Result};

use crate::core::chatbot::openai::OpenAiClient;
use crate::core::llm::{LlmModelRecord, LlmProviderRecord};
use crate::core::llm::providers::{RemoteLlmModelInfo, extra_with_reasoning};
use crate::core::transcribe::TranscribeModelRecord;
use crate::core::transcribe::openai_audio::OpenAiAudioTranscriber;
use crate::core::tts::TtsModelRecord;
use crate::core::tts::openai_tts::OpenAiTtsSynthesiser;
use crate::core::provider::{ApiProvider, BuiltLlmClient, ProviderField, ProviderUiMeta, ReasoningMode, ServiceType};

pub struct OpenAiProvider;

#[async_trait::async_trait]
impl ApiProvider for OpenAiProvider {
    fn type_id(&self) -> &'static str { "open_ai" }
    fn display_name(&self) -> &'static str { "OpenAI" }
    fn supported_types(&self) -> &'static [ServiceType] {
        &[ServiceType::Llm, ServiceType::Transcribe, ServiceType::Tts]
    }

    async fn list_llm_models(&self, _record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        Ok(None)
    }

    fn reasoning_mode(&self, model_id: &str, capabilities: &[String]) -> Option<ReasoningMode> {
        // Reasoning ("o" series and other reasoning models) → effort levels.
        let id = model_id.to_lowercase();
        let is_reasoning = capabilities.iter().any(|c| c == "reasoning")
            || id.starts_with("o1") || id.starts_with("o3") || id.starts_with("o4")
            || id.starts_with("gpt-5");
        if is_reasoning {
            Some(ReasoningMode::ValueSet {
                values:  vec!["low".to_string(), "medium".to_string(), "high".to_string()],
                default: Some("medium".to_string()),
            })
        } else {
            None
        }
    }

    fn reasoning_request(&self, value: &serde_json::Value) -> Option<serde_json::Value> {
        let effort = value.as_str()?;
        Some(serde_json::json!({ "reasoning_effort": effort }))
    }

    fn build_llm(&self, record: &LlmProviderRecord, model: &LlmModelRecord) -> Option<Result<BuiltLlmClient>> {
        Some((|| {
            let key = record.api_key.as_deref()
                .with_context(|| format!("provider '{}': api_key required for open_ai", record.name))?;
            let extra = extra_with_reasoning(self, model);
            Ok(BuiltLlmClient {
                client: Arc::new(OpenAiClient::new("https://api.openai.com/v1", key, extra, false)),
                prompt_cache: false,
            })
        })())
    }

    fn build_tts(&self, record: &LlmProviderRecord, model: &TtsModelRecord) -> Option<Result<Arc<dyn crate::core::tts::TextToSpeech>>> {
        Some((|| {
            let base_url = record.base_url.clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let api_key = record.api_key.clone()
                .with_context(|| format!("provider '{}': api_key required for open_ai", record.name))?;
            Ok(Arc::new(OpenAiTtsSynthesiser::new(
                &model.name, base_url, api_key, &model.model_id,
                model.voice_id.clone(), model.instructions.clone(), model.response_format.clone(),
            )) as Arc<dyn crate::core::tts::TextToSpeech>)
        })())
    }

    fn build_transcriber(&self, record: &LlmProviderRecord, model: &TranscribeModelRecord) -> Option<Result<Arc<dyn crate::core::transcribe::Transcribe>>> {
        Some((|| {
            let base_url = record.base_url.clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let api_key = record.api_key.clone()
                .with_context(|| format!("provider '{}': api_key required for open_ai", record.name))?;
            Ok(Arc::new(OpenAiAudioTranscriber::new(
                &model.name, base_url, api_key, &model.model_id, model.language.clone(),
            )) as Arc<dyn crate::core::transcribe::Transcribe>)
        })())
    }

    fn ui_meta(&self) -> ProviderUiMeta {
        ProviderUiMeta {
            type_id:      "open_ai",
            display_name: "OpenAI",
            description:  None,
            color:        "#10a37f",
            icon:         "bi-lightning-charge",
            fields: &[
                ProviderField { key: "api_key", label: "API Key", required: true,  secret: true  },
                ProviderField { key: "base_url", label: "Base URL (optional)",  required: false, secret: false },
            ],
        }
    }
}
