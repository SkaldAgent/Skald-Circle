//! Typed, fire-and-forget event seam for a running agent turn.
//!
//! Every event a turn produces used to be sent inline as
//! `tx.send(ServerEvent::X { .. }).await.ok()`, scattered across `llm_loop`,
//! `resume`, `agent_dispatch`, and `approval`. `TurnEmitter` wraps the per-turn
//! `mpsc::Sender<ServerEvent>` (which `ChatHub` bridges onto the global broadcast
//! bus) and exposes one semantic method per event, so the loop speaks in domain
//! terms (`emitter.tool_done(..)`) instead of constructing wire enums by hand.
//!
//! It is a zero-cost borrow wrapper: construct one at the top of a function that
//! emits and pass `&TurnEmitter` to any helper. This is also the single seam a
//! future event-bus / UI-vs-domain split would hook into.

use serde_json::Value;
use tokio::sync::mpsc;

use core_api::message_meta::Attachment;

use crate::events::ServerEvent;

/// Borrows the per-turn event sender and emits typed [`ServerEvent`]s.
pub(super) struct TurnEmitter<'a> {
    tx: &'a mpsc::Sender<ServerEvent>,
}

impl<'a> TurnEmitter<'a> {
    pub(super) fn new(tx: &'a mpsc::Sender<ServerEvent>) -> Self {
        Self { tx }
    }

    /// Send an event, dropping it silently if the receiver is gone (the same
    /// `.await.ok()` semantics every call site used before).
    async fn emit(&self, event: ServerEvent) {
        self.tx.send(event).await.ok();
    }

    // ── User / assistant turn events ────────────────────────────────────────

    /// A user message row was persisted (telnet-style echo).
    pub(super) async fn user_message(&self, message_id: i64, content: String, attachments: Vec<Attachment>) {
        self.emit(ServerEvent::UserMessage { message_id, content, attachments }).await;
    }

    /// The assistant produced text alongside tool calls (reasoning before acting).
    pub(super) async fn thinking(&self, message_id: i64, content: String, input_tokens: Option<u32>, output_tokens: Option<u32>) {
        self.emit(ServerEvent::Thinking { message_id, content, input_tokens, output_tokens }).await;
    }

    /// The assistant response is complete.
    pub(super) async fn done(&self, message_id: i64, stack_id: i64, content: String, input_tokens: Option<u32>, output_tokens: Option<u32>) {
        self.emit(ServerEvent::Done { message_id, stack_id, content, input_tokens, output_tokens }).await;
    }

    /// The LLM was cut off by the token limit.
    pub(super) async fn truncated(&self, output_tokens: Option<u32>) {
        self.emit(ServerEvent::Truncated { output_tokens }).await;
    }

    /// A fatal error occurred processing the request.
    pub(super) async fn error(&self, message: String) {
        self.emit(ServerEvent::Error { message }).await;
    }

    // ── Tool-call lifecycle ─────────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn tool_start(
        &self,
        tool_call_id: i64,
        message_id: i64,
        name: String,
        arguments: Value,
        display_name: String,
        icon: String,
        label_short: String,
        label_full: String,
        path: Option<String>,
    ) {
        self.emit(ServerEvent::ToolStart {
            tool_call_id, message_id, name, arguments, display_name, icon, label_short, label_full, path,
        }).await;
    }

    pub(super) async fn tool_done(
        &self,
        tool_call_id: i64,
        result: String,
        result_type: String,
        preview_old: Option<String>,
        preview_new: Option<String>,
    ) {
        self.emit(ServerEvent::ToolDone { tool_call_id, result, result_type, preview_old, preview_new }).await;
    }

    pub(super) async fn tool_error(&self, tool_call_id: i64, error: String) {
        self.emit(ServerEvent::ToolError { tool_call_id, error }).await;
    }

    pub(super) async fn tool_cancelled(&self, tool_call_id: i64) {
        self.emit(ServerEvent::ToolCancelled { tool_call_id }).await;
    }

    pub(super) async fn tool_rejected(&self, tool_call_id: i64, reason: String) {
        self.emit(ServerEvent::ToolRejected { tool_call_id, reason }).await;
    }

    /// A file-write tool completed; ask clients holding the file to reload.
    pub(super) async fn file_changed(&self, path: String) {
        self.emit(ServerEvent::FileChanged { path }).await;
    }

    // ── Approval / clarification prompts ────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn pending_write(
        &self,
        request_id: i64,
        tool_call_id: i64,
        path: String,
        old_content: Option<String>,
        new_content: String,
    ) {
        self.emit(ServerEvent::PendingWrite { request_id, tool_call_id, path, old_content, new_content }).await;
    }

    pub(super) async fn approval_required(&self, request_id: i64, tool_call_id: i64, tool_name: String, arguments: Value) {
        self.emit(ServerEvent::ApprovalRequired { request_id, tool_call_id, tool_name, arguments }).await;
    }

    // Note: `AgentQuestion` is emitted directly in `dispatch_ask_user_clarification`
    // because that one site inspects the send Result for diagnostic logging — it is
    // deliberately not wrapped here.

    // ── Sub-agent stack frames ──────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn agent_start(
        &self,
        stack_id: i64,
        parent_tool_call_id: i64,
        agent_id: String,
        parent_agent_id: String,
        depth: i64,
        prompt_preview: String,
    ) {
        self.emit(ServerEvent::AgentStart {
            stack_id, parent_tool_call_id, agent_id, parent_agent_id, depth, prompt_preview,
        }).await;
    }

    pub(super) async fn agent_done(&self, stack_id: i64, agent_id: String, parent_agent_id: String, result_preview: String) {
        self.emit(ServerEvent::AgentDone { stack_id, agent_id, parent_agent_id, result_preview }).await;
    }

    // ── LLM model fallback ──────────────────────────────────────────────────

    pub(super) async fn model_fallback(&self, from: String, to: String, reason: String) {
        self.emit(ServerEvent::ModelFallback { from, to, reason }).await;
    }

    pub(super) async fn llm_failed(&self, tried: Vec<String>, last_error: String) {
        self.emit(ServerEvent::LlmFailed { tried, last_error }).await;
    }
}
