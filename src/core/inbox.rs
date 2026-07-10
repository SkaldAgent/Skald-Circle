use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use serde_json::Value;

use core_api::inbox::{
    InboxApi, InboxApprovalItem, InboxClarificationItem, InboxElicitationItem, InboxSnapshot,
};
use core_api::tool::ToolDescriptionLength;

use crate::core::approval::{ApprovalManager, PendingApprovalInfo};
use crate::core::clarification::{ClarificationManager, PendingClarificationInfo};
use crate::core::elicitation::{ElicitationManager, ElicitationOutcome, PendingElicitationInfo};
use crate::core::tools::ToolRegistry;

#[derive(Serialize)]
pub struct InboxItems {
    pub total:          usize,
    pub approvals:      Vec<PendingApprovalInfo>,
    pub clarifications: Vec<PendingClarificationInfo>,
    pub elicitations:   Vec<PendingElicitationInfo>,
}

#[derive(Clone)]
pub struct Inbox {
    pub approval:  Arc<ApprovalManager>,
    clarification: Arc<ClarificationManager>,
    elicitation:   Arc<ElicitationManager>,
    /// Used to humanise approval tool calls (`describe`) when building snapshots.
    tools:         Arc<ToolRegistry>,
}

impl Inbox {
    pub fn new(
        approval:      Arc<ApprovalManager>,
        clarification: Arc<ClarificationManager>,
        elicitation:   Arc<ElicitationManager>,
        tools:         Arc<ToolRegistry>,
    ) -> Self {
        Self { approval, clarification, elicitation, tools }
    }

    pub async fn list_pending(&self) -> InboxItems {
        let mut approvals = self.approval.list_pending().await;
        // Union in DB-persisted pending approvals not represented in memory, so the
        // Inbox survives a server restart (the registry is in-memory only). Both sources
        // key on the durable `tool_call_id` (live approvals now carry
        // `request_id == tool_call_id`; persisted ones carry the falsy
        // `PERSISTED_REQUEST_ID`, telling the client to resolve by `tool_call_id`), so the
        // dedup below is a single-id-space set difference.
        let live: std::collections::HashSet<i64> =
            approvals.iter().map(|a| a.tool_call_id).collect();
        for a in self.approval.list_persisted_pending().await {
            if !live.contains(&a.tool_call_id) {
                approvals.push(a);
            }
        }
        let clarifications = self.clarification.list_pending().await;
        let elicitations   = self.elicitation.list_pending().await;
        let total          = approvals.len() + clarifications.len() + elicitations.len();
        InboxItems { total, approvals, clarifications, elicitations }
    }

    pub async fn approve(&self, request_id: i64) {
        self.approval.approve(request_id).await;
    }

    pub async fn reject(&self, request_id: i64, note: String) {
        self.approval.reject(request_id, note).await;
    }

    pub async fn answer(&self, request_id: i64, answer: String) -> bool {
        self.clarification.resolve(request_id, answer).await
    }

    pub async fn resolve_elicitation(&self, request_id: i64, action: String, content: Option<Value>) -> bool {
        self.elicitation.resolve(request_id, ElicitationOutcome { action, content }).await
    }
}

/// Exposes the Inbox to plugins via `PluginContext` (plugin.md §12.2). Converts
/// the main-crate pending types into the core-api snapshot types.
#[async_trait]
impl InboxApi for Inbox {
    async fn list_pending(&self) -> InboxSnapshot {
        let items = self.list_pending().await;
        let approvals = items.approvals.into_iter().map(|a| {
            // Humanise the tool call for the card / notification; ship the raw
            // arguments untruncated so the detail dialog shows exactly what is
            // being approved (e.g. the full `execute_cmd` command).
            let summary = self.tools.describe_call(&a.tool_name, &a.arguments, ToolDescriptionLength::Short);
            InboxApprovalItem {
                request_id:    a.request_id,
                tool_name:     a.tool_name,
                summary,
                arguments:     a.arguments,
                agent_id:      a.agent_id,
                source:        a.source,
                context_label: a.context_label,
                created_at:    a.created_at,
            }
        }).collect();
        let clarifications = items.clarifications.into_iter().map(|c| InboxClarificationItem {
            request_id:        c.request_id,
            agent_id:          c.agent_id,
            source:            c.source,
            context_label:     c.context_label,
            title:             c.title,
            question:          c.question,
            suggested_answers: c.suggested_answers,
            created_at:        c.created_at,
        }).collect();
        let elicitations = items.elicitations.into_iter().map(|e| InboxElicitationItem {
            request_id:      e.request_id,
            server_name:     e.server_name,
            message:         e.message,
            field_name:      e.field_name,
            sensitive:       e.sensitive,
            is_confirmation: e.is_confirmation,
            created_at:      e.created_at,
        }).collect();
        InboxSnapshot { total: items.total, approvals, clarifications, elicitations }
    }

    async fn approve(&self, request_id: i64) {
        self.approve(request_id).await;
    }

    async fn reject(&self, request_id: i64, reason: String) {
        self.reject(request_id, reason).await;
    }

    async fn answer(&self, request_id: i64, answer: String) -> bool {
        self.answer(request_id, answer).await
    }

    async fn resolve_elicitation(&self, request_id: i64, action: String, content: Option<Value>) -> bool {
        self.resolve_elicitation(request_id, action, content).await
    }
}
