//! Inbox API exposed to plugins.
//!
//! The Skald `Inbox` façade (src/core/inbox.rs) wraps the `ApprovalManager` and
//! `ClarificationManager`. Plugins receive `Arc<dyn InboxApi>` via `PluginContext`
//! and use it to read pending items and resolve them, without depending on the
//! main crate. `request_id` is the integer rowid used both for resolution and
//! for idempotency (plugin.md §2): `approve`/`reject`/`answer` on an already
//! resolved id are no-ops.

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;

/// One pending approval, surfaced to plugins (mirrors the main crate's
/// `PendingApprovalInfo`, trimmed to the fields a plugin needs for the
/// `inbox_update` payload — see payloads.md §3.1).
#[derive(Debug, Clone, Serialize)]
pub struct InboxApprovalItem {
    pub request_id:    i64,
    pub tool_name:     String,
    /// Human-readable one-line label for the tool call, from `Tool::describe(Short)`.
    /// Used for the card / push notification text.
    pub summary:       String,
    /// Raw tool arguments (untruncated). Source of truth for the detail dialog so
    /// the user sees exactly what is being approved — critical for `execute_cmd`.
    pub arguments:     Value,
    pub agent_id:      String,
    pub source:        String,
    pub context_label: Option<String>,
    /// ISO-8601 timestamp string (UTC).
    pub created_at:    String,
}

/// One pending clarification, surfaced to plugins (mirrors the main crate's
/// `PendingClarificationInfo`).
#[derive(Debug, Clone, Serialize)]
pub struct InboxClarificationItem {
    pub request_id:        i64,
    pub agent_id:          String,
    pub source:            String,
    pub context_label:     Option<String>,
    pub title:             String,
    pub question:          String,
    pub suggested_answers: Vec<String>,
    /// ISO-8601 timestamp string (UTC).
    pub created_at:        String,
}

/// One pending elicitation, surfaced to plugins (mirrors the main crate's
/// `PendingElicitationInfo`). Holds only prompt metadata — never the value.
#[derive(Debug, Clone, Serialize)]
pub struct InboxElicitationItem {
    pub request_id:      i64,
    pub server_name:     String,
    pub message:         String,
    pub field_name:      Option<String>,
    pub sensitive:       bool,
    pub is_confirmation: bool,
    /// ISO-8601 timestamp string (UTC).
    pub created_at:      String,
}

/// A snapshot of all pending Inbox items.
#[derive(Debug, Clone, Serialize)]
pub struct InboxSnapshot {
    pub total:          usize,
    pub approvals:      Vec<InboxApprovalItem>,
    pub clarifications: Vec<InboxClarificationItem>,
    pub elicitations:   Vec<InboxElicitationItem>,
}

/// Inbox operations available to plugins.
#[async_trait]
pub trait InboxApi: Send + Sync {
    /// Snapshot of all currently pending approvals + clarifications.
    async fn list_pending(&self) -> InboxSnapshot;

    /// Approve a pending tool-call request. No-op if already resolved.
    async fn approve(&self, request_id: i64);

    /// Reject a pending tool-call request with a reason. No-op if already resolved.
    async fn reject(&self, request_id: i64, reason: String);

    /// Answer a pending clarification. Returns `true` if a pending entry was
    /// found and resolved, `false` otherwise (idempotent).
    async fn answer(&self, request_id: i64, answer: String) -> bool;

    /// Resolve a pending MCP elicitation. `action` is `"accept"`/`"decline"`/
    /// `"cancel"`; `content` carries the field values for `accept` (it may hold
    /// a secret — do not log it). Returns `true` if a pending entry was found.
    async fn resolve_elicitation(&self, request_id: i64, action: String, content: Option<Value>) -> bool;
}
