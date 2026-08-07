//! The `Model` trait (a stateless LLM client), the `ModelSelector` seam
//! (selection + health), and the shipped selectors.
//!
//! `Model` is the boundary the kernel talks to; the shipped clients live in
//! [`crate::models`]. The wire format at this boundary is OpenAI-shaped
//! `serde_json::Value` (blueprint D4) — the Anthropic client translates
//! internally.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::activation::ToolRendering;
use crate::ids::{ConversationId, FrameId, ModelId};

// ── Usage ────────────────────────────────────────────────────────────────────

/// Token/cost accounting of one model call. All fields optional: providers
/// report different subsets (or nothing, e.g. Ollama cost).
#[derive(Debug, Default, Clone)]
pub struct Usage {
    pub input_tokens:  Option<u32>,
    pub output_tokens: Option<u32>,
    pub cache_read:    Option<u32>,
    pub cache_write:   Option<u32>,
    pub cost_usd:      Option<f64>,
    /// The model stopped at the token limit (`finish_reason == "length"` /
    /// `stop_reason == "max_tokens"`).
    pub truncated:     bool,
}

impl Usage {
    pub fn is_present(&self) -> bool {
        self.input_tokens.is_some() || self.output_tokens.is_some()
    }
}

// ── ToolCall ─────────────────────────────────────────────────────────────────

/// A tool call requested by the model (wire level).
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// The provider's call id ("call_abc", "toolu_01…"). May be empty for
    /// providers that don't assign one — the assembler then synthesizes one.
    pub id:        String,
    pub name:      String,
    pub arguments: Value,
}

// ── StreamDelta ──────────────────────────────────────────────────────────────

/// An incremental piece of a streaming completion. Best-effort UI feedback:
/// senders use `try_send` and drop deltas when the channel is full — streaming
/// must never backpressure the HTTP read. The returned [`ModelResponse`]
/// remains the only authoritative result.
#[derive(Debug, Clone)]
pub enum StreamDelta {
    Text(String),
    Reasoning(String),
}

// ── RawMeta ──────────────────────────────────────────────────────────────────

/// Raw HTTP metadata captured during a provider call, for host-side payload
/// logging (a `LoggingModel` decorator persists it). Sensitive header values
/// are redacted by the clients before capture.
#[derive(Debug, Default, Clone)]
pub struct RawMeta {
    pub request_headers:  Option<Value>,
    pub request_body:     Option<Value>,
    pub response_headers: Option<Value>,
    pub response_body:    Option<Value>,
}

// ── ModelResponse ────────────────────────────────────────────────────────────

/// The authoritative outcome of one model call.
#[derive(Debug, Clone)]
pub enum ModelResponse {
    Message {
        content:   String,
        reasoning: Option<String>,
        usage:     Usage,
        raw:       Option<RawMeta>,
    },
    ToolCalls {
        content:   String,
        calls:     Vec<ToolCall>,
        reasoning: Option<String>,
        usage:     Usage,
        raw:       Option<RawMeta>,
    },
}

impl ModelResponse {
    pub fn message(content: impl Into<String>) -> Self {
        Self::Message { content: content.into(), reasoning: None, usage: Usage::default(), raw: None }
    }

    pub fn tool_calls(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self::ToolCalls { content: content.into(), calls, reasoning: None, usage: Usage::default(), raw: None }
    }

    pub fn usage(&self) -> &Usage {
        match self {
            Self::Message { usage, .. } | Self::ToolCalls { usage, .. } => usage,
        }
    }

    pub fn usage_mut(&mut self) -> &mut Usage {
        match self {
            Self::Message { usage, .. } | Self::ToolCalls { usage, .. } => usage,
        }
    }

    pub fn content(&self) -> &str {
        match self {
            Self::Message { content, .. } | Self::ToolCalls { content, .. } => content,
        }
    }

    pub fn reasoning(&self) -> Option<&str> {
        match self {
            Self::Message { reasoning, .. } | Self::ToolCalls { reasoning, .. } => {
                reasoning.as_deref()
            }
        }
    }

    pub fn raw(&self) -> Option<&RawMeta> {
        match self {
            Self::Message { raw, .. } | Self::ToolCalls { raw, .. } => raw.as_ref(),
        }
    }
}

// ── ModelError ───────────────────────────────────────────────────────────────

/// A structured model-call failure. The HTTP status lives in the type, never
/// in a substring of the message — a model id or token count containing
/// "404" must not mis-classify retriability.
#[derive(Debug, Clone)]
pub struct ModelError {
    /// HTTP status, when the failure came from an HTTP response. `None` for
    /// network/parse/cancellation failures — callers treat those as retriable.
    pub status:  Option<u16>,
    pub message: String,
    /// Request/response payload captured at the failing call, so the host's
    /// debug log can show what was actually sent even when the provider
    /// rejected it. `None` when there was no HTTP round-trip.
    pub raw:     Option<RawMeta>,
}

impl ModelError {
    pub fn new(status: Option<u16>, message: impl Into<String>) -> Self {
        Self { status, message: message.into(), raw: None }
    }

    pub fn with_raw(mut self, raw: RawMeta) -> Self {
        self.raw = Some(raw);
        self
    }

    pub fn from_reqwest(err: reqwest::Error) -> Self {
        let status = err.status().map(|s| s.as_u16());
        Self { status, message: err.to_string(), raw: None }
    }
}

impl std::fmt::Display for ModelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "[HTTP {s}] {}", self.message),
            None    => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ModelError {}

// ── ModelRequest ─────────────────────────────────────────────────────────────

/// One model call. `messages`/`tools` are OpenAI-shaped wire values (D4).
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages:    Vec<Value>,
    pub tools:       Vec<Value>,
    /// Concrete model name ("kimi-k3", "claude-sonnet-4-5", …).
    pub model:       String,
    pub max_tokens:  Option<u32>,
    pub temperature: Option<f32>,
    /// Correlation id minted by the kernel at every attempt — for host-side
    /// logging/telemetry only, ignored by the kernel itself.
    pub request_id:  String,
    pub conversation: ConversationId,
    pub frame:       FrameId,
    /// Host free-form per-request extras (e.g. reasoning knobs resolved for
    /// this model). Merged last by the shipped clients INTO THE REQUEST BODY.
    pub extras:      Value,
    /// Host logging/telemetry correlation (session ids, user id, …).
    /// **Never** merged into the request body by the shipped clients — it
    /// exists for host decorators (e.g. a `LoggingModel`) only.
    pub log:         Option<Value>,
}

// ── Model ────────────────────────────────────────────────────────────────────

/// A stateless LLM client. Implementations hold only connection config (base
/// URL, API key). No memory, no database, no session state.
#[async_trait]
pub trait Model: Send + Sync {
    /// One completion. `deltas` is a best-effort side-channel for streaming:
    /// implementations push [`StreamDelta`]s via `try_send` and never block on
    /// it. The returned [`ModelResponse`] is the only authoritative result.
    ///
    /// Shipped clients retry the call buffered when the stream fails before
    /// any delta was emitted (providers rejecting `stream` keep working); a
    /// mid-stream failure propagates to the caller's fallback logic.
    async fn complete(
        &self,
        req:    &ModelRequest,
        deltas: Option<mpsc::Sender<StreamDelta>>,
    ) -> Result<ModelResponse, ModelError>;

    /// Retriability classification **for this model**. Default — the crate
    /// owns the protocols (blueprint D13): 401/403/404/422 are NOT retriable;
    /// 400/429/5xx and status-less failures (network, parse, cancel) are.
    /// Hosts may override via a wrapping `Model`.
    fn is_retriable(&self, err: &ModelError) -> bool {
        !matches!(err.status, Some(401 | 403 | 404 | 422))
    }
}

// ── ModelInfo / ModelHandle ──────────────────────────────────────────────────

/// Metadata influencing build/serialization. Read by assemblers and `ToolSet`,
/// NEVER interpreted by the kernel (it passes them through).
#[derive(Debug, Clone, Default)]
pub struct ModelInfo {
    /// Anthropic-style prompt-cache hints.
    pub prompt_cache:   bool,
    /// "vision", "video", "tool_search", …
    pub capabilities:   Vec<String>,
    /// Dynamic-tool-loading wire protocol (blueprint §4.10). Default `Inline`.
    pub tool_rendering: ToolRendering,
    /// Host free-form (Skald: context_length, extra_params).
    pub extras:         Value,
}

impl ModelInfo {
    pub fn has_capability(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }
}

/// A selected model plus its metadata, as returned by a `ModelSelector`.
#[derive(Clone)]
pub struct ModelHandle {
    pub id:    ModelId,
    pub model: Arc<dyn Model>,
    pub info:  ModelInfo,
    /// Wire model name when it differs from `id`: a selector whose `id` is a
    /// bookkeeping key (Skald: the user-facing alias keying its model
    /// registry) sets this to the provider's API model id. `None` ⇒ `id`
    /// goes on the wire.
    pub wire_id: Option<ModelId>,
}

impl ModelHandle {
    /// The model identifier to put on the wire.
    pub fn wire_model(&self) -> &str {
        self.wire_id.as_deref().unwrap_or(&self.id)
    }
}

// ── ModelHint ────────────────────────────────────────────────────────────────

/// Selection hint: only the explicit pin (blueprint D14). Strength/tiering/
/// priority are host logic, resolved inside the host's `ModelSelector`.
#[derive(Debug, Clone, Default)]
pub struct ModelHint {
    /// Explicit model pin — bypasses the host's AUTO selection.
    pub name: Option<ModelId>,
}

impl ModelHint {
    pub fn name(name: impl Into<ModelId>) -> Self {
        Self { name: Some(name.into()) }
    }
}

// ── ModelSelector ────────────────────────────────────────────────────────────

/// The selection seam. The kernel calls `select` once per round and again on
/// every fallback (`exclude` = models already tried in this round).
#[async_trait]
pub trait ModelSelector: Send + Sync {
    async fn select(&self, hint: &ModelHint, exclude: &[ModelId]) -> crate::Result<ModelHandle>;

    /// Health reporting — default no-op. Hosts back these with circuit
    /// breakers / status dashboards (Skald: LlmManager mark_success/failure).
    async fn report_success(&self, _id: &ModelId) {}
    async fn report_failure(&self, _id: &ModelId, _err: &str) {}
}

// ── RetryPolicy ──────────────────────────────────────────────────────────────

/// Fallback budget per round: how many DISTINCT models to try before
/// `LlmFailed`. Retriability classification lives on `Model::is_retriable`.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: usize,
}

impl Default for RetryPolicy {
    fn default() -> Self { Self { max_attempts: 3 } }
}

// ── Shipped selectors ────────────────────────────────────────────────────────

/// One model, no fallback. Pair it with a shipped client
/// (`models::OpenAiModel::new(...)`) for a complete agent in ~50 lines.
pub struct SingleModel {
    handle: ModelHandle,
}

impl SingleModel {
    pub fn new(model: impl NamedModel) -> Self {
        Self { handle: model.into_handle() }
    }

    pub fn with_info(model: impl NamedModel, info: ModelInfo) -> Self {
        let mut handle = model.into_handle();
        handle.info = info;
        Self { handle }
    }

    pub fn from_handle(handle: ModelHandle) -> Self { Self { handle } }
}

#[async_trait]
impl ModelSelector for SingleModel {
    async fn select(&self, _hint: &ModelHint, _exclude: &[ModelId]) -> crate::Result<ModelHandle> {
        Ok(self.handle.clone())
    }
}

/// A model with a self-assigned selector id — implemented by every shipped
/// client (the id defaults to the client's `default_model()`).
pub trait NamedModel: Model + 'static {
    /// Selector id and default wire model name for this client.
    fn default_model(&self) -> &str;

    fn into_handle(self) -> ModelHandle
    where
        Self: Sized,
    {
        ModelHandle {
            id:      self.default_model().to_string(),
            model:   Arc::new(self),
            info:    ModelInfo::default(),
            wire_id: None,
        }
    }
}

/// An ordered list of models: the first non-excluded entry wins, so the list
/// order IS the fallback order (blueprint D14 — "an ordered list given at
/// construction"). `hint.name` pins a list entry by id.
pub struct StaticModels {
    handles: Vec<ModelHandle>,
    cursor:  AtomicUsize,
}

impl StaticModels {
    pub fn new(handles: Vec<ModelHandle>) -> Self {
        assert!(!handles.is_empty(), "StaticModels requires at least one model");
        Self { handles, cursor: AtomicUsize::new(0) }
    }

    pub fn from_clients(models: Vec<impl NamedModel>) -> Self {
        Self::new(models.into_iter().map(|m| m.into_handle()).collect())
    }
}

#[async_trait]
impl ModelSelector for StaticModels {
    async fn select(&self, hint: &ModelHint, exclude: &[ModelId]) -> crate::Result<ModelHandle> {
        // Explicit pin on the first selection of a round: resolve by id.
        // (A non-empty `exclude` means the pinned model already failed:
        // fall through to the ordered list.)
        if let Some(name) = &hint.name
            && exclude.is_empty()
        {
            return self
                .handles
                .iter()
                .find(|h| &h.id == name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("unknown pinned model '{name}'"));
        }
        // Rotation start so concurrent conversations don't pile onto handle[0].
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % self.handles.len();
        self.handles
            .iter()
            .cycle()
            .skip(start)
            .take(self.handles.len())
            .find(|h| !exclude.iter().any(|e| e == &h.id))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no alternative models available (all excluded)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_retriability_classifies_on_status() {
        struct M;
        #[async_trait]
        impl Model for M {
            async fn complete(
                &self,
                _req: &ModelRequest,
                _d: Option<mpsc::Sender<StreamDelta>>,
            ) -> Result<ModelResponse, ModelError> {
                unreachable!()
            }
        }
        let m = M;
        for non_retriable in [401, 403, 404, 422] {
            assert!(
                !m.is_retriable(&ModelError::new(Some(non_retriable), "x")),
                "{non_retriable} must not retry"
            );
        }
        for retriable in [400, 429, 500, 502, 503] {
            assert!(
                m.is_retriable(&ModelError::new(Some(retriable), "x")),
                "{retriable} must retry"
            );
        }
        assert!(m.is_retriable(&ModelError::new(None, "network down")));
    }

    #[test]
    fn model_hint_is_only_a_pin() {
        let h = ModelHint::name("kimi-k3");
        assert_eq!(h.name.as_deref(), Some("kimi-k3"));
        assert!(ModelHint::default().name.is_none());
    }
}
