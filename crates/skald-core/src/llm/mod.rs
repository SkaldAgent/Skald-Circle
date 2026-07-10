pub(crate) mod db;
pub mod manager;
pub mod providers;

use std::sync::Arc;

use crate::chatbot::ChatbotClient;
use crate::provider::ServiceType;

pub use core_api::provider::{LlmProviderRecord, LlmModelRecord, LlmStrength, ReasoningMode};
pub use manager::{LlmManager, sort_models_for_agent};

/// A resolved, ready-to-use LLM client with its associated metadata.
#[derive(Clone)]
pub struct LlmEntry {
    pub client:          Arc<dyn ChatbotClient>,
    pub model:           String,
    pub model_db_id:     i64,
    pub strength:        Option<LlmStrength>,
    pub scope:           Vec<String>,
    pub extra_params:    Option<serde_json::Value>,
    /// Max input context window in tokens, if known.
    pub context_length:  Option<i64>,
    /// When true, prompt-caching hints are injected into requests.
    pub prompt_cache:    bool,
}

// ── Provider ──────────────────────────────────────────────────────────────────

/// Public provider metadata (no api_key).
#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmProviderInfo {
    pub id:              i64,
    pub name:            String,
    #[serde(rename = "type")]
    pub provider:        String,
    pub base_url:        Option<String>,
    pub description:     Option<String>,
    /// Service types this provider supports (from ProviderRegistry at runtime).
    pub supported_types: Vec<ServiceType>,
}

/// Public model metadata for API responses (includes provider name for convenience).
#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmModelInfo {
    pub id:                       i64,
    pub provider_id:              i64,
    pub provider_name:            String,
    pub model_id:                 String,
    pub name:                     String,
    pub strength:                 Option<LlmStrength>,
    pub scope:                    Vec<String>,
    pub is_default:               bool,
    pub priority:                 i32,
    pub extra_params:             Option<serde_json::Value>,
    pub context_length:           Option<i64>,
    pub max_output_tokens:        Option<i64>,
    pub knowledge_cutoff:         Option<String>,
    pub capabilities:             Vec<String>,
    pub status:                   ClientStatus,
    pub last_error:               Option<String>,
    /// Input (prompt) price per million tokens (USD) from the provider catalog cache.
    pub price_input_per_million:  Option<f64>,
    /// Output (completion) price per million tokens (USD) from the provider catalog cache.
    pub price_output_per_million: Option<f64>,
    /// Currently-selected reasoning value (string for a `ValueSet`, number for a
    /// `Range`, or `None`). Round-trips to the edit form.
    pub reasoning:                Option<serde_json::Value>,
    /// Reasoning control descriptor for this model (drives the UI control), or
    /// `None` if the model does not support reasoning.
    pub reasoning_mode:           Option<ReasoningMode>,
}

// ── Health ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientStatus {
    Healthy,
    Degraded,
    Down,
}
