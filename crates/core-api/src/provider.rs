use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use agent_loop::model::Model;

use crate::image_generate::{ImageGenerate, ImageGenerateModelRecord};
use crate::tts::{TextToSpeech, TtsModelRecord, RemoteTtsModelInfo};
use crate::transcribe::{Transcribe, TranscribeModelRecord, RemoteTranscribeModelInfo};

// ── LlmStrength ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmStrength {
    VeryLow,
    Low,
    Average,
    High,
    VeryHigh,
}

// ── Provider record types (DB ↔ manager) ──────────────────────────────────────

/// Full provider record (includes secrets — only expose over trusted local connections).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LlmProviderRecord {
    pub id:          i64,
    pub name:        String,
    /// Provider type_id string as stored in DB (e.g. "open_ai", "anthropic").
    #[serde(rename = "type")]
    pub provider:    String,
    pub api_key:     Option<String>,
    /// Only used by ollama and lm_studio.
    pub base_url:    Option<String>,
    pub description: Option<String>,
}

/// Full model record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LlmModelRecord {
    pub id:                i64,
    pub provider_id:       i64,
    pub model_id:          String,
    pub name:              String,
    pub strength:          Option<LlmStrength>,
    pub is_default:        bool,
    pub priority:          i32,
    pub extra_params:      Option<serde_json::Value>,
    pub context_length:    Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub knowledge_cutoff:  Option<String>,
    pub capabilities:      Vec<String>,
    /// Selected reasoning value (interpreted per provider via `ReasoningMode`):
    /// a JSON string for a `ValueSet` (e.g. `"high"`, `"enabled"`) or a JSON
    /// number for a `Range` (e.g. `8000`). `None` = reasoning off / unset.
    pub reasoning:         Option<serde_json::Value>,
}

/// Remote model info returned by a provider's `list_llm_models()`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RemoteLlmModelInfo {
    pub id:                       String,
    pub name:                     String,
    pub context_length:           Option<u64>,
    pub max_completion_tokens:    Option<u64>,
    pub knowledge_cutoff:         Option<String>,
    pub capabilities:             Vec<String>,
    pub vision:                   Option<bool>,
    pub price_input_per_million:  Option<f64>,
    pub price_output_per_million: Option<f64>,
    /// Reasoning control descriptor for this model, if it supports reasoning.
    /// Filled by the API layer from `ApiProvider::reasoning_mode`; providers'
    /// `list_llm_models` may leave it `None`.
    #[serde(default)]
    pub reasoning:                Option<ReasoningMode>,
}

// ── ReasoningMode ─────────────────────────────────────────────────────────────

/// Describes how a model exposes its "reasoning"/"thinking" knob, so the UI can
/// render the right control and the caller knows the space of valid values.
/// Provider-owned (each `ApiProvider` returns it per model); the actual request
/// translation is done by `ApiProvider::reasoning_request`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningMode {
    /// A fixed set of discrete string choices — e.g. `["low","medium","high"]`
    /// (effort levels) or `["disabled","enabled"]` (on/off toggle).
    ValueSet {
        values:  Vec<String>,
        default: Option<String>,
    },
    /// A continuous numeric range — e.g. Anthropic thinking `budget_tokens`.
    Range {
        min:     i64,
        max:     i64,
        step:    Option<i64>,
        default: Option<i64>,
        unit:    Option<String>,
    },
}

// ── ServiceType ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceType {
    Llm,
    Transcribe,
    ImageGenerate,
    Tts,
}

// ── UI metadata ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderUiMeta {
    pub type_id:      &'static str,
    pub display_name: &'static str,
    pub description:  Option<&'static str>,
    pub color:        &'static str,
    pub icon:         &'static str,
    pub fields:       &'static [ProviderField],
    /// Whether the provider can list its remote LLM catalog
    /// (`list_llm_models` is implemented). Drives the frontend's model picker —
    /// no provider type_id is ever hardcoded in the UI.
    pub lists_models: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ProviderField {
    pub key:      &'static str,
    pub label:    &'static str,
    pub required: bool,
    pub secret:   bool,
}

// ── BuiltLlmClient ────────────────────────────────────────────────────────────

pub struct BuiltLlmClient {
    /// A stateless `agent_loop` model client (blueprint D13).
    pub client:       Arc<dyn Model>,
    pub prompt_cache: bool,
}

// ── ApiProvider trait ─────────────────────────────────────────────────────────

#[async_trait]
pub trait ApiProvider: Send + Sync {
    /// Short stable identifier stored in the DB (e.g. "open_ai", "anthropic").
    fn type_id(&self) -> &'static str;
    fn display_name(&self) -> &'static str;
    fn supported_types(&self) -> &'static [ServiceType];

    async fn list_llm_models(
        &self,
        _record: &LlmProviderRecord,
    ) -> Result<Option<Vec<RemoteLlmModelInfo>>> {
        Ok(None)
    }

    /// Reasoning control descriptor for a given model, or `None` if the model
    /// does not support reasoning. Drives the UI control (a `ValueSet` dropdown
    /// or a numeric `Range`). Provider-owned so each provider decides which of
    /// its models support reasoning and in what form.
    fn reasoning_mode(
        &self,
        _model_id:     &str,
        _capabilities: &[String],
    ) -> Option<ReasoningMode> {
        None
    }

    /// Translates a selected reasoning `value` (see `LlmModelRecord::reasoning`)
    /// into the provider-specific JSON fragment to merge into the request body
    /// (e.g. `{"reasoning":{"effort":"high"}}` or `{"thinking":{"type":"enabled"}}`).
    /// `None` = nothing to inject.
    fn reasoning_request(
        &self,
        _value: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        None
    }

    /// The dynamic-tool-loading (DTL) serialization format this provider's models
    /// speak, e.g. `"anthropic_tool_reference"` or `"kimi_system_tools"`. Applied
    /// only to a model that opts in via the `tool_search` capability. `None` = no
    /// DTL (activated tools ride in the top-level `tools`). Returned as a string so
    /// core-api needs no dependency on the engine's `DtlMode` — the caller parses it.
    fn dtl_format(&self) -> Option<&str> {
        None
    }

    async fn llm_model_info(
        &self,
        _record:   &LlmProviderRecord,
        _model_id: &str,
    ) -> Result<Option<RemoteLlmModelInfo>> {
        Ok(None)
    }

    async fn list_tts_models(
        &self,
        _record: &LlmProviderRecord,
    ) -> Result<Option<Vec<RemoteTtsModelInfo>>> {
        Ok(None)
    }

    async fn list_transcribe_models(
        &self,
        _record: &LlmProviderRecord,
    ) -> Result<Option<Vec<RemoteTranscribeModelInfo>>> {
        Ok(None)
    }

    fn build_llm(
        &self,
        _record: &LlmProviderRecord,
        _model:  &LlmModelRecord,
    ) -> Option<Result<BuiltLlmClient>> {
        None
    }

    fn build_tts(
        &self,
        _record: &LlmProviderRecord,
        _model:  &TtsModelRecord,
    ) -> Option<Result<Arc<dyn TextToSpeech>>> {
        None
    }

    fn build_transcriber(
        &self,
        _record: &LlmProviderRecord,
        _model:  &TranscribeModelRecord,
    ) -> Option<Result<Arc<dyn Transcribe>>> {
        None
    }

    fn build_image_generator(
        &self,
        _record: &LlmProviderRecord,
        _model:  &ImageGenerateModelRecord,
    ) -> Option<Result<Arc<dyn ImageGenerate>>> {
        None
    }

    fn ui_meta(&self) -> ProviderUiMeta;
}

// ── ApiProviderRegistry trait ─────────────────────────────────────────────────

/// Write-side of the provider registry: register and remove plugin-provided
/// `ApiProvider` implementations at runtime.
///
/// Implemented by `ProviderRegistry` in the main crate. Plugins that supply
/// their own API provider (e.g. `plugin-elevenlabs`) use this to register at
/// start and unregister at stop.
#[async_trait]
pub trait ApiProviderRegistry: Send + Sync {
    fn register_plugin(&self, provider: Arc<dyn ApiProvider>);
    fn unregister_plugin(&self, type_id: &str);
}
