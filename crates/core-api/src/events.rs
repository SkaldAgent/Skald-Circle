use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message_meta::Attachment;

// ── Client → Server ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ClientMessage {
    pub content: String,
    /// Files attached to this message (uploaded beforehand via `POST /api/{source}/uploads`).
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// Typed data push from remote clients (iOS app, etc.).
/// Sent over the existing WebSocket as `{"type":"data","stream":"...","payload":{...}}`.
#[derive(Deserialize)]
pub struct InboundDataMessage {
    pub stream:  String,
    pub payload: Value,
}

// ── Global event envelope ─────────────────────────────────────────────────────

/// Envelope that wraps every event on the global broadcast bus.
/// `source` is `None` for system/background events (cron, tic, plugins).
#[derive(Clone)]
pub struct GlobalEvent {
    pub source:     Option<String>,
    pub session_id: Option<i64>,
    pub event:      ServerEvent,
}

// ── Server → Client ───────────────────────────────────────────────────────────

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    /// A tool call was started. DB status: running.
    ToolStart {
        tool_call_id: i64,
        message_id:   i64,
        name:         String,
        arguments:    Value,
        /// Concise human-readable label (≤60 chars): tool + primary argument.
        label_short:  String,
        /// Verbose human-readable label (≤120 chars): tool + all meaningful arguments.
        label_full:   String,
        /// Path to a single viewable file this call targets, if any. The
        /// frontend renders it as a clickable link to the file viewer.
        #[serde(skip_serializing_if = "Option::is_none")]
        path:         Option<String>,
    },
    /// A tool call completed successfully. DB status: done.
    ToolDone {
        tool_call_id: i64,
        result:       String,
        /// Result type tag: `"string"` (plain text) or `"json"` (structured
        /// payload, e.g. MCP `structuredContent`). The frontend uses it to render
        /// typed results instead of a raw text blob. Always populated by the
        /// server; the frontend treats an absent/unknown value as plain text, so
        /// older clients degrade gracefully.
        result_type:  String,
    },
    /// A tool call failed. DB status: error.
    ToolError {
        tool_call_id: i64,
        error:        String,
    },
    /// A tool call was stopped by the user via `/stop`. DB status: cancelled.
    /// Distinct from `ToolError`: a cancellation is deliberate, not a failure.
    ToolCancelled {
        tool_call_id: i64,
    },
    /// A tool call was denied by an approval policy or a human. DB status: rejected.
    /// Distinct from `ToolError`: a denial is a policy decision, not a failure.
    ToolRejected {
        tool_call_id: i64,
        reason:       String,
    },
    /// A sub-agent stack frame was opened.
    AgentStart {
        stack_id:            i64,
        parent_tool_call_id: i64,
        agent_id:            String,
        parent_agent_id:     String,
        depth:               i64,
        /// The prompt sent to the sub-agent (truncated to 500 chars by the sender).
        prompt_preview:      String,
    },
    /// A sub-agent stack frame was closed.
    AgentDone {
        stack_id: i64,
        agent_id: String,
        parent_agent_id: String,
        /// The sub-agent's final response (truncated to 500 chars by the sender).
        result_preview: String,
    },
    /// The assistant response is complete.
    Done {
        message_id:    i64,
        stack_id:      i64,
        content:       String,
        input_tokens:  Option<u32>,
        output_tokens: Option<u32>,
    },
    /// A fatal error occurred processing the request.
    Error {
        message: String,
    },
    /// The LLM was cut off by the token limit (finish_reason="length").
    Truncated {
        output_tokens: Option<u32>,
    },
    /// The LLM produced text alongside tool calls (reasoning before acting).
    Thinking {
        message_id:    i64,
        content:       String,
        input_tokens:  Option<u32>,
        output_tokens: Option<u32>,
    },
    /// A write operation requires user approval before executing (shows a diff).
    PendingWrite {
        request_id:   i64,
        tool_call_id: i64,
        path:         String,
        old_content:  Option<String>,
        new_content:  String,
    },
    /// A non-file tool call requires user approval before executing.
    /// Used for MCP tools, execute_cmd, restart, and any other tool
    /// that the ApprovalManager flags as `Require`.
    ApprovalRequired {
        request_id:   i64,
        tool_call_id: i64,
        tool_name:    String,
        arguments:    Value,
    },
    /// A sub-agent needs clarification from the user before continuing.
    AgentQuestion {
        request_id:        i64,
        tool_call_id:      i64,
        title:             String,
        question:          String,
        suggested_answers: Vec<String>,
    },
    /// A book file was written by a tool; the frontend should reload if it has it open.
    FileChanged {
        path: String,
    },
    /// Ask the frontend to open a file for the user. Behaves like
    /// `window.openFile(path)`: navigates to the file viewer page for markdown /
    /// text / images, or opens an HTML file in a new browser tab. Emitted by
    /// the future `show_file_to_user` interface tool (not wired yet).
    OpenFile {
        path: String,
    },
    /// The active LLM model failed and the system switched to a fallback automatically.
    ModelFallback {
        from:   String,
        to:     String,
        reason: String,
    },
    /// All LLM fallback attempts were exhausted; the turn could not complete.
    LlmFailed {
        tried:      Vec<String>,
        last_error: String,
    },
    /// A new approval entered the Inbox. Emitted on the global bus when the
    /// `ApprovalManager` registers a pending request, so bus subscribers (e.g.
    /// the mobile-connector plugin) can re-snapshot the Inbox. Distinct from
    /// `ApprovalRequired`, which is the per-session WS event carrying full args
    /// for the active client.
    ApprovalRequested {
        request_id:   i64,
        tool_call_id: i64,
        tool_name:    String,
    },
    /// A pending approval or pending-write was resolved (approved or rejected).
    /// Emitted on the global bus so all clients (e.g. Telegram) can update their UI.
    ApprovalResolved {
        request_id:   i64,
        tool_call_id: i64,
        approved:     bool,
    },
    /// A new clarification entered the Inbox. Emitted on the global bus when the
    /// `ClarificationManager` registers a pending question. Distinct from
    /// `AgentQuestion`, which is the per-session WS event for the active client.
    ClarificationRequested {
        request_id: i64,
        title:      String,
    },
    /// A pending clarification was resolved (answered). Emitted on the global bus
    /// so all clients can update their Inbox view.
    ClarificationResolved {
        request_id: i64,
    },
    /// A server-initiated MCP elicitation entered the Inbox (e.g. an MCP server
    /// asking for a sudo password mid tool-call). Carries only `request_id` +
    /// `title` — never the requested value.
    ElicitationRequested {
        request_id: i64,
        title:      String,
    },
    /// A pending elicitation was resolved (accepted / declined / cancelled).
    /// Emitted on the global bus so all clients can update their Inbox view.
    ElicitationResolved {
        request_id: i64,
    },
    /// The active session for a source was replaced (e.g. /new, /clear).
    NewSession {
        session_id: i64,
    },
    /// A user message was persisted to history; broadcast so every client (the
    /// sender included) renders the bubble. Emitted at save time — when the row
    /// is appended, either at turn start or at a round boundary for messages
    /// injected mid-turn — so the bubble appears exactly where the agent saw it.
    /// Clients render purely from this echo (no optimistic local rendering).
    UserMessage {
        /// Id of the `chat_history` row just written.
        message_id: i64,
        content: String,
        /// Files attached to the message; lets secondary clients render chips live.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    /// Sent to a client right after it (re)connects, reporting whether a turn is
    /// currently in flight for its session. Lets a reloaded page restore the
    /// SEND→STOP button state instead of assuming idle.
    TurnRunning {
        running: bool,
    },
    /// The selected LLM client (model) for a source changed. Broadcast to every
    /// client of the source so dropdowns/selects stay in sync. `client` is a
    /// `client_names()` entry (typically `"auto"` or a model name). Driven by
    /// `ChatHub::set_selected_client` — the backend is the single source of truth.
    ClientSelected {
        client: String,
    },
}

impl ServerEvent {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ServerEvent serialization failed")
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Self::ToolStart      { .. } => "tool_start",
            Self::ToolDone       { .. } => "tool_done",
            Self::ToolError      { .. } => "tool_error",
            Self::ToolCancelled  { .. } => "tool_cancelled",
            Self::ToolRejected   { .. } => "tool_rejected",
            Self::AgentStart     { .. } => "agent_start",
            Self::AgentDone      { .. } => "agent_done",
            Self::Done           { .. } => "done",
            Self::Error          { .. } => "error",
            Self::Thinking       { .. } => "thinking",
            Self::PendingWrite       { .. } => "pending_write",
            Self::ApprovalRequired   { .. } => "approval_required",
            Self::AgentQuestion      { .. } => "agent_question",
            Self::FileChanged        { .. } => "file_changed",
            Self::OpenFile           { .. } => "open_file",
            Self::Truncated          { .. } => "truncated",
            Self::ModelFallback      { .. } => "model_fallback",
            Self::LlmFailed          { .. } => "llm_failed",
            Self::ApprovalRequested  { .. } => "approval_requested",
            Self::ApprovalResolved   { .. } => "approval_resolved",
            Self::ClarificationRequested { .. } => "clarification_requested",
            Self::ClarificationResolved  { .. } => "clarification_resolved",
            Self::ElicitationRequested   { .. } => "elicitation_requested",
            Self::ElicitationResolved    { .. } => "elicitation_resolved",
            Self::NewSession         { .. } => "new_session",
            Self::UserMessage        { .. } => "user_message",
            Self::TurnRunning        { .. } => "turn_running",
            Self::ClientSelected     { .. } => "client_selected",
        }
    }
}
