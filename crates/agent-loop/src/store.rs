//! `HistoryStore` — the durability heart of the loop.
//!
//! Contract (enforced by doc, relied upon by recovery):
//!
//! 1. **Every state transition is an immediate write** — the kernel never
//!    accumulates state in RAM. A crash loses only RAM, never truth.
//! 2. `MessageId`/`ToolCallId` are **monotonically increasing per frame**.
//! 3. `resolve_call` is the ONLY path to terminal states; `set_call_state`
//!    is only for `Running → AwaitingHuman`.
//! 4. `load` returns calls nested inside their messages — the input of the
//!    assembler's well-formed projection.

use async_trait::async_trait;
use serde_json::Value;

use crate::ids::{ConversationId, FrameId, MessageId, SummaryId, ToolCallId};
use crate::model::Usage;

// ── Role ─────────────────────────────────────────────────────────────────────

/// Who produced a message. `Agent` is an injected agent-to-agent message
/// (sub-agent prompt, async result delivery); it projects to `user` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
    Agent,
}

// ── CallState ────────────────────────────────────────────────────────────────

/// Lifecycle of a tool call — semantics identical to Skald's
/// `chat_llm_tools.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallState {
    /// Was executing at crash time → interrupted (NOT terminal).
    Running,
    /// 'pending': approval or clarification in flight (NOT terminal).
    AwaitingHuman,
    /// Terminal.
    Done,
    /// Terminal.
    Failed,
    /// Deliberate /stop — NEVER re-execute.
    Cancelled,
    /// Policy/human denial — NEVER re-execute.
    Rejected,
}

impl CallState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled | Self::Rejected)
    }
}

// ── CallOutcome ──────────────────────────────────────────────────────────────

/// The result of an execution, before recording.
#[derive(Debug, Clone)]
pub enum CallOutcome {
    Completed(crate::tool::ToolOutput),
    Failed(String),
    Cancelled,
    Rejected { reason: String },
}

impl CallOutcome {
    pub fn state(&self) -> CallState {
        match self {
            Self::Completed(_) => CallState::Done,
            Self::Failed(_)    => CallState::Failed,
            Self::Cancelled    => CallState::Cancelled,
            Self::Rejected { .. } => CallState::Rejected,
        }
    }

    /// Text persisted as the call's result. Kept RAW (the assembler formats
    /// for the model: `Failed` results get their "Error:" prefix at
    /// projection time, not here) so hosts with an existing schema (Skald's
    /// `chat_llm_tools.result`) round-trip byte-identically.
    pub fn result_text(&self) -> String {
        match self {
            Self::Completed(out) => out.to_wire(),
            Self::Failed(e)      => e.clone(),
            Self::Cancelled      => "Cancelled by user.".to_string(),
            Self::Rejected { reason } => reason.clone(),
        }
    }

    pub fn result_kind(&self) -> &'static str {
        match self {
            Self::Completed(out) => out.kind(),
            Self::Failed(_)      => "error",
            Self::Cancelled      => "cancelled",
            Self::Rejected { .. } => "rejected",
        }
    }
}

// ── Frames ───────────────────────────────────────────────────────────────────

/// What a frame is opened with (a sub-agent dispatch; the root carries the
/// conversation's entry agent).
#[derive(Debug, Clone)]
pub struct FrameSpec {
    /// Agent id in the HOST's catalog (opaque to the crate).
    pub agent:       String,
    /// The sub-agent's prompt (root: None).
    pub prompt:      Option<String>,
    pub depth:       u32,
    /// The parent frame's tool call that spawned this frame.
    pub parent_call: Option<ToolCallId>,
    /// Host free-form (run_context_json, …).
    pub meta:        Value,
}

impl FrameSpec {
    pub fn root(agent: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            prompt: None,
            depth: 0,
            parent_call: None,
            meta: Value::Null,
        }
    }
}

/// A stored frame.
#[derive(Debug, Clone)]
pub struct FrameRecord {
    pub id:     FrameId,
    pub conversation: ConversationId,
    pub parent: Option<FrameId>,
    pub spec:   FrameSpec,
    pub active: bool,
}

// ── Messages ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub role:      Role,
    pub content:   String,
    /// TIC/notify/injection: not echoed to the UI as a user message.
    pub synthetic: bool,
    pub reasoning: Option<String>,
    /// Attachments, command display, … (host free-form).
    pub metadata:  Option<Value>,
}

impl NewMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into(), synthetic: false, reasoning: None, metadata: None }
    }

    pub fn assistant(content: impl Into<String>, reasoning: Option<String>) -> Self {
        Self { role: Role::Assistant, content: content.into(), synthetic: false, reasoning, metadata: None }
    }

    pub fn agent(content: impl Into<String>) -> Self {
        Self { role: Role::Agent, content: content.into(), synthetic: false, reasoning: None, metadata: None }
    }

    pub fn synthetic(mut self, synthetic: bool) -> Self {
        self.synthetic = synthetic;
        self
    }

    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A stored message with its tool calls nested.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id:        MessageId,
    pub role:      Role,
    pub content:   String,
    pub reasoning: Option<String>,
    pub synthetic: bool,
    /// Orphan of a cancelled turn — excluded from `load`.
    pub failed:    bool,
    pub metadata:  Option<Value>,
    pub usage:     Usage,
    pub calls:     Vec<StoredCall>,
}

// ── Tool calls ───────────────────────────────────────────────────────────────

/// What a call is recorded with, BEFORE execution (phase 1 of the fan-out).
#[derive(Debug, Clone)]
pub struct NewCall {
    /// The model's wire call id ("call_abc", "toolu_…"), needed to rebuild
    /// `tool_calls`/`tool` wire messages. Synthesized by the store when absent.
    pub provider_id: Option<String>,
    pub name:        String,
    pub arguments:   Value,
}

impl NewCall {
    pub fn new(name: impl Into<String>, arguments: Value) -> Self {
        Self { provider_id: None, name: name.into(), arguments }
    }

    pub fn with_provider_id(mut self, id: impl Into<String>) -> Self {
        self.provider_id = Some(id.into());
        self
    }
}

/// A stored tool call.
#[derive(Debug, Clone)]
pub struct StoredCall {
    pub id:          ToolCallId,
    pub message_id:  MessageId,
    /// The model's wire call id (see [`NewCall::provider_id`]).
    pub provider_id: String,
    pub name:        String,
    pub arguments:   Value,
    /// The arguments **exactly as the model emitted them**, when the store kept
    /// the string. The projection replays this verbatim: re-serializing
    /// [`Self::arguments`] reorders object keys, which changes the bytes the
    /// model produced and breaks the prompt-cache prefix.
    pub arguments_raw: Option<String>,
    pub state:       CallState,
    pub result:      Option<String>,
    pub result_kind: String,
    /// Host free-form (Skald: preview_old/new, media refs).
    pub extras:      Value,
}

// ── Summaries ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct NewSummary {
    pub text:          String,
    /// Last message covered by the summary — the projection resumes after it.
    pub covered_up_to: MessageId,
}

#[derive(Debug, Clone)]
pub struct StoredSummary {
    pub id:            SummaryId,
    pub text:          String,
    pub covered_up_to: MessageId,
}

// ── HistoryStore ─────────────────────────────────────────────────────────────

#[async_trait]
pub trait HistoryStore: Send + Sync {
    // ── frames ──
    async fn open_frame(
        &self,
        conv:   &ConversationId,
        parent: Option<FrameId>,
        spec:   FrameSpec,
    ) -> crate::Result<FrameId>;
    async fn close_frame(&self, frame: FrameId) -> crate::Result<()>;
    /// One frame by id (DelegateTool depth checks, recovery).
    async fn get_frame(&self, frame: FrameId) -> crate::Result<Option<FrameRecord>>;
    /// All active frames of a conversation (recovery: batch detection, cascade).
    async fn active_frames(&self, conv: &ConversationId) -> crate::Result<Vec<FrameRecord>>;
    async fn deepest_active(&self, conv: &ConversationId) -> crate::Result<Option<FrameRecord>>;

    // ── messages ──
    async fn append(&self, frame: FrameId, msg: NewMessage) -> crate::Result<MessageId>;
    async fn set_usage(&self, msg: MessageId, usage: &Usage) -> crate::Result<()>;
    /// Frame history with calls nested per message. EXCLUDES failed messages
    /// (orphans of cancelled turns).
    async fn load(&self, frame: FrameId) -> crate::Result<Vec<StoredMessage>>;
    async fn load_since(&self, frame: FrameId, after: MessageId) -> crate::Result<Vec<StoredMessage>>;
    async fn last(&self, frame: FrameId) -> crate::Result<Option<StoredMessage>>;
    async fn mark_failed(&self, msg: MessageId) -> crate::Result<()>;

    // ── tool calls ──
    async fn append_call(&self, msg: MessageId, call: NewCall) -> crate::Result<ToolCallId>;
    /// The ONLY path to terminal states.
    async fn resolve_call(&self, id: ToolCallId, outcome: &CallOutcome) -> crate::Result<()>;
    /// Only `Running → AwaitingHuman`.
    async fn set_call_state(&self, id: ToolCallId, state: CallState) -> crate::Result<()>;
    /// One call by id (translators enriching finish events, recovery).
    async fn get_call(&self, id: ToolCallId) -> crate::Result<Option<StoredCall>>;
    /// The frame a call belongs to. Recovery walks the cascade with it, and an
    /// out-of-band resolution (an approval answered from a REST endpoint) has
    /// nothing but a call id to start from.
    async fn frame_of_call(&self, id: ToolCallId) -> crate::Result<Option<FrameRecord>>;
    /// Merge host free-form extras into a call (Skald: diff preview, media).
    /// Keys not understood by the store are ignored.
    async fn set_call_extras(&self, id: ToolCallId, extras: Value) -> crate::Result<()>;
    async fn calls_in_state(&self, frame: FrameId, states: &[CallState]) -> crate::Result<Vec<StoredCall>>;

    // ── summaries ──
    async fn save_summary(&self, frame: FrameId, s: NewSummary) -> crate::Result<SummaryId>;
    async fn latest_summary(&self, frame: FrameId) -> crate::Result<Option<StoredSummary>>;
}
