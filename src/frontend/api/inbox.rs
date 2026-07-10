use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

use std::sync::Arc;
use crate::core::skald::Skald;
use super::ApiError;

// ── GET /api/inbox ────────────────────────────────────────────────────────────
//
// Returns all pending approval requests and clarification requests in a single
// response, so the frontend can show a unified Agent Inbox page with one fetch.

pub async fn list(State(skald): State<Arc<Skald>>) -> Json<Value> {
    let items = skald.inbox().list_pending().await;
    Json(json!({
        "total":          items.total,
        "approvals":      items.approvals,
        "clarifications": items.clarifications,
        "elicitations":   items.elicitations,
    }))
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
    Path(p):      Path<ApprovePath>,
    Json(body):   Json<ApproveBody>,
) -> Result<Json<Value>, ApiError> {
    // Peek info before resolving so we have session_id and tool metadata for bypass.
    let info = skald.approval().get_pending(p.request_id).await;

    if body.action == "reject" {
        // Pass the raw note; the waiting session builds the canonical message.
        skald.inbox().reject(p.request_id, body.note.clone()).await;
    } else {
        skald.inbox().approve(p.request_id).await;

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
                        skald.approval().bypass_session_for_category(info.session_id, cat, duration).await;
                    } else {
                        apply_all_bypass(&skald, info.session_id, duration).await;
                    }
                }
                "mcp_server" => {
                    if let Some(server) = info.mcp_server {
                        skald.approval().bypass_session_for_mcp(info.session_id, server, duration).await;
                    } else {
                        apply_all_bypass(&skald, info.session_id, duration).await;
                    }
                }
                _ => apply_all_bypass(&skald, info.session_id, duration).await,
            }
        }
    }
    Ok(Json(json!({ "ok": true, "request_id": p.request_id, "action": body.action })))
}

async fn apply_all_bypass(skald: &Skald, session_id: i64, duration: Option<Duration>) {
    match duration {
        Some(d) => skald.approval().bypass_session_for(session_id, d).await,
        None    => skald.approval().bypass_session(session_id).await,
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
    Path(p):      Path<ClarifyPath>,
    Json(body):   Json<ClarifyBody>,
) -> Result<Json<Value>, ApiError> {
    if body.answer.trim().is_empty() {
        return Err(ApiError::bad_request("answer must not be empty"));
    }
    let resolved = skald.inbox().answer(p.request_id, body.answer).await;
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
    Path(p):      Path<ElicitPath>,
    Json(body):   Json<ElicitBody>,
) -> Result<Json<Value>, ApiError> {
    let action = match body.action.as_str() {
        "accept" | "decline" | "cancel" => body.action.clone(),
        other => return Err(ApiError::bad_request(format!("invalid action: {other}"))),
    };
    let resolved = skald.inbox().resolve_elicitation(p.request_id, action.clone(), body.content).await;
    if resolved {
        Ok(Json(json!({ "ok": true, "request_id": p.request_id, "action": action })))
    } else {
        Err(ApiError::not_found("elicitation request not found"))
    }
}
