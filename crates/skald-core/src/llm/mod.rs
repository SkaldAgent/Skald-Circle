pub(crate) mod db;
pub mod logging;
pub mod manager;
pub mod providers;

use std::sync::Arc;

use agent_loop::model::Model;

use crate::provider::ServiceType;

pub use core_api::provider::{LlmProviderRecord, LlmModelRecord, LlmStrength, ReasoningMode};
pub use manager::{LlmManager, sort_models_for_agent};

/// A resolved, ready-to-use LLM client with its associated metadata.
#[derive(Clone)]
pub struct LlmEntry {
    pub client:          Arc<dyn Model>,
    pub model:           String,
    pub model_db_id:     i64,
    pub strength:        Option<LlmStrength>,
    pub extra_params:    Option<serde_json::Value>,
    /// Max input context window in tokens, if known.
    pub context_length:  Option<i64>,
    /// When true, prompt-caching hints are injected into requests.
    pub prompt_cache:    bool,
    /// Input capabilities of the resolved model (`vision`, `video`, …), from
    /// `llm_models.capabilities`. Drives multimodal attachment inlining.
    pub capabilities:    Vec<String>,
    /// Dynamic-tool-loading serialization mode for this model (resolved from
    /// `capabilities` + provider type). Selects how a session's *activated* tools
    /// are put on the wire so that activating one does not invalidate the
    /// provider's prompt-cache prefix.
    pub dtl:             DtlMode,
}

/// Per-model dynamic-tool-loading (DTL) serialization mode. Resolved in
/// `build_entry` from the model's provider (via [`dtl_mode_from_format`]) gated by
/// the `tool_search` capability. It selects how a session's activated tools are serialized so
/// that an `activate_tools` call does not break the provider's prompt-cache
/// prefix. The persistence layer (`activated_tools`) is model-agnostic; this is
/// the model-aware half that renders that state per provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DtlMode {
    /// Today's behaviour: activated tools ride in the top-level `tools` array.
    /// Correct, but every activation invalidates the cache from that point on.
    /// The fallback for Ollama / LM Studio / generic OpenAI-compat providers.
    #[default]
    None,
    /// Anthropic Messages API: candidate tools are declared `defer_loading:true`
    /// and loaded via a custom client-side `tool_reference` expansion emitted at
    /// the `activate_tools` result (preserves the cache; no 5-result cap).
    AnthropicToolReference,
    /// Kimi K3 (OpenAI-compatible): activated tools are injected as `system`
    /// messages carrying a `tools` field, appended at the activation position so
    /// the prefix stays byte-identical (append-only).
    KimiSystemTools,
}

/// Parses a provider-declared DTL format name — from a native provider
/// (`AnthropicProvider::dtl_format`) or from `providers.yaml` (`dtl:` on a declared
/// provider) — into a [`DtlMode`]. Unknown names → [`DtlMode::None`].
///
/// The *format* is a property of the provider (which wire its client speaks);
/// whether a given model *uses* it is gated separately by the `tool_search`
/// capability (see `build_entry`). So there is no hardcoded model list — enabling
/// a new Kimi-compatible provider is a `providers.yaml` edit.
pub fn dtl_mode_from_format(fmt: &str) -> DtlMode {
    match fmt {
        "anthropic_tool_reference" => DtlMode::AnthropicToolReference,
        "kimi_system_tools"        => DtlMode::KimiSystemTools,
        _                          => DtlMode::None,
    }
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
