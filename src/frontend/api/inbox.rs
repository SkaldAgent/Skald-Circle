use axum::{
    Json, Extension,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

use std::sync::Arc;
use skald_core::skald::{Skald, UserContext};
use super::{ApiError, guard::AuthUser, require_context};

// ── GET /api/inbox ────────────────────────────────────────────────────────────
//
// Returns all pending approval requests and clarification requests in a single
// response, so the frontend can show a unified Agent Inbox page with one fetch.

pub async fn list(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let items = ctx.inbox.list_pending().await;
    Ok(Json(json!({
        "total":          items.total,
        "approvals":      items.approvals,
        "clarifications": items.clarifications,
        "elicitations":   items.elicitations,
    })))
}

// ── GET /api/{source}/inbox ───────────────────────────────────────────────────
//
// The pending items raised by the background tasks *this* conversation started.
//
// An async sub-agent runs in a session of its own (`source = "cron"`), so its
// `ApprovalRequired` / `AgentQuestion` events — the rich, per-session ones that
// draw the inline card — never reach the chat's WebSocket, and until now the
// only place they surfaced was the Inbox. The chat already shows what it handed
// off (the background-task strip); this is the same question asked of the
// pending items, so the strip can carry the card too.
//
// The join is server-side on purpose: "whose is this pending item" is the same
// question `/{source}/tasks` answers for a task, and it should have one answer.
// The client is left with a list to render, not a correlation to guess.
//
// Live updates ride the `approval_requested` / `approval_resolved` /
// `clarification_*` events, which are already forwarded to every one of this
// user's sockets regardless of source — they carry ids only, so this endpoint
// is what turns a nudge into something renderable, and what a page reload reads.
//
// Elicitations are absent: `PendingElicitationInfo` carries no `session_id`, so
// there is nothing to attribute one to a task with.

#[derive(serde::Serialize)]
pub struct TaskInboxApproval {
    /// The task that is asking — the strip labels the card with it.
    pub job_id:    i64,
    pub job_title: String,
    #[serde(flatten)]
    pub item:      skald_core::approval::PendingApprovalInfo,
}

#[derive(serde::Serialize)]
pub struct TaskInboxClarification {
    pub job_id:    i64,
    pub job_title: String,
    #[serde(flatten)]
    pub item:      skald_core::clarification::PendingClarificationInfo,
}

#[derive(serde::Serialize)]
pub struct TaskInbox {
    pub approvals:      Vec<TaskInboxApproval>,
    pub clarifications: Vec<TaskInboxClarification>,
}

pub async fn session_task_inbox(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<super::cron::SourcePath>,
) -> Result<Json<TaskInbox>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    // A chat that has never run has no session, and therefore no tasks. Not an
    // error: the strip asks on every load, including the first one.
    let Some(session_id) = skald_core::db::sources::active_session_id(&ctx.pool, &p.source).await? else {
        return Ok(Json(TaskInbox { approvals: vec![], clarifications: vec![] }));
    };
    task_inbox_of_session(&ctx, session_id).await
}

/// The same inbox, addressed by conversation — what an extra copilot tab asks for.
pub async fn session_task_inbox_by_id(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<i64>,
) -> Result<Json<TaskInbox>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    task_inbox_of_session(&ctx, id).await
}

async fn task_inbox_of_session(
    ctx: &skald_core::skald::UserContext,
    session_id: i64,
) -> Result<Json<TaskInbox>, ApiError> {
    let empty = TaskInbox { approvals: vec![], clarifications: vec![] };
    let children =
        skald_core::db::scheduled_jobs::running_child_sessions(&ctx.pool, session_id).await?;
    if children.is_empty() {
        return Ok(Json(empty));
    }
    let by_session: std::collections::HashMap<i64, &skald_core::db::scheduled_jobs::RunningChildSession> =
        children.iter().map(|c| (c.session_id, c)).collect();

    let items = ctx.inbox.list_pending().await;
    let approvals = items.approvals.into_iter()
        .filter_map(|item| by_session.get(&item.session_id).map(|job| TaskInboxApproval {
            job_id:    job.job_id,
            job_title: job.title.clone(),
            item,
        }))
        .collect();
    let clarifications = items.clarifications.into_iter()
        .filter_map(|item| by_session.get(&item.session_id).map(|job| TaskInboxClarification {
            job_id:    job.job_id,
            job_title: job.title.clone(),
            item,
        }))
        .collect();

    Ok(Json(TaskInbox { approvals, clarifications }))
}

// ── POST /api/inbox/approvals/:request_id/resolve ─────────────────────────────

#[derive(Deserialize)]
pub struct ApprovePath { pub request_id: i64 }

#[derive(Deserialize)]
pub struct ApproveBody {
    #[serde(default = "default_action")]
    pub action: String,
    #[serde(default)]
    pub note: String,
    /// Seconds for the bypass duration. `0` means indefinite (session-scoped).
    /// Absent means no bypass.
    pub bypass_secs: Option<u64>,
    /// `"category"` | `"mcp_server"` | `"all"`. Defaults to auto-detect from tool info.
    pub bypass_scope: Option<String>,
}

fn default_action() -> String { "approve".to_string() }

pub async fn resolve_approval(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p):      Path<ApprovePath>,
    Json(body):   Json<ApproveBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    // Peek info before resolving so we have session_id and tool metadata for bypass.
    let info = ctx.approval.get_pending(p.request_id).await;

    if body.action == "reject" {
        // Pass the raw note; the waiting session builds the canonical message.
        ctx.inbox.reject(p.request_id, body.note.clone()).await;
    } else {
        ctx.inbox.approve(p.request_id).await;

        // Apply bypass if requested (only on approve).
        if let (Some(info), Some(bypass_secs)) = (info, body.bypass_secs) {
            let duration = if bypass_secs == 0 { None } else { Some(Duration::from_secs(bypass_secs)) };

            let scope = body.bypass_scope.as_deref().unwrap_or_else(|| {
                if info.tool_category.is_some() { "category" }
                else if info.mcp_server.is_some() { "mcp_server" }
                else { "all" }
            });

            match scope {
                "category" => {
                    if let Some(cat) = info.tool_category {
                        ctx.approval.bypass_session_for_category(info.session_id, cat, duration).await;
                    } else {
                        apply_all_bypass(&ctx, info.session_id, duration).await;
                    }
                }
                "mcp_server" => {
                    if let Some(server) = info.mcp_server {
                        ctx.approval.bypass_session_for_mcp(info.session_id, server, duration).await;
                    } else {
                        apply_all_bypass(&ctx, info.session_id, duration).await;
                    }
                }
                _ => apply_all_bypass(&ctx, info.session_id, duration).await,
            }
        }
    }
    Ok(Json(json!({ "ok": true, "request_id": p.request_id, "action": body.action })))
}

async fn apply_all_bypass(ctx: &UserContext, session_id: i64, duration: Option<Duration>) {
    match duration {
        Some(d) => ctx.approval.bypass_session_for(session_id, d).await,
        None    => ctx.approval.bypass_session(session_id).await,
    }
}

// ── POST /api/inbox/clarifications/:request_id/resolve ────────────────────────

#[derive(Deserialize)]
pub struct ClarifyPath { pub request_id: i64 }

#[derive(Deserialize)]
pub struct ClarifyBody {
    pub answer: String,
}

pub async fn resolve_clarification(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p):      Path<ClarifyPath>,
    Json(body):   Json<ClarifyBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    if body.answer.trim().is_empty() {
        return Err(ApiError::bad_request("answer must not be empty"));
    }
    let resolved = ctx.inbox.answer(p.request_id, body.answer).await;
    if resolved {
        Ok(Json(json!({ "ok": true, "request_id": p.request_id })))
    } else {
        Err(ApiError::not_found("clarification request not found"))
    }
}

// ── POST /api/inbox/elicitations/:request_id/resolve ──────────────────────────
//
// Resolve a server-initiated MCP elicitation. `action` is "accept"/"decline"/
// "cancel"; on "accept", `content` carries the field values (e.g. a password).
// The value is forwarded to the MCP server and is never logged or persisted.

#[derive(Deserialize)]
pub struct ElicitPath { pub request_id: i64 }

#[derive(Deserialize)]
pub struct ElicitBody {
    #[serde(default = "default_elicit_action")]
    pub action:  String,
    #[serde(default)]
    pub content: Option<Value>,
}

fn default_elicit_action() -> String { "decline".to_string() }

pub async fn resolve_elicitation(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p):      Path<ElicitPath>,
    Json(body):   Json<ElicitBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let action = match body.action.as_str() {
        "accept" | "decline" | "cancel" => body.action.clone(),
        other => return Err(ApiError::bad_request(format!("invalid action: {other}"))),
    };
    let resolved = ctx.inbox.resolve_elicitation(p.request_id, action.clone(), body.content).await;
    if resolved {
        Ok(Json(json!({ "ok": true, "request_id": p.request_id, "action": action })))
    } else {
        Err(ApiError::not_found("elicitation request not found"))
    }
}
