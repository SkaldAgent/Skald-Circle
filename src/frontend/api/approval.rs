use axum::{
    Json,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::core::approval::NewApprovalRule;
use crate::core::tool_catalog::{AllTools, McpServerMeta, ToolInfo};
use std::collections::HashSet;
use std::sync::Arc;
use crate::core::skald::Skald;

use super::ApiError;

// ── GET /api/approval/rules ───────────────────────────────────────────────────

pub async fn list_rules(
    State(skald): State<Arc<Skald>>,
) -> Result<Json<Value>, ApiError> {
    let rules = skald.approval().list_rules().await?;
    Ok(Json(json!(rules)))
}

// ── POST /api/approval/rules ──────────────────────────────────────────────────

pub async fn create_rule(
    State(skald): State<Arc<Skald>>,
    Json(body): Json<NewApprovalRule>,
) -> Result<Json<Value>, ApiError> {
    let id = skald.approval().add_rule(body).await?;
    Ok(Json(json!({ "id": id })))
}

// ── PUT /api/approval/rules/:id ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RulePath { pub id: i64 }

pub async fn update_rule(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<RulePath>,
    Json(body): Json<NewApprovalRule>,
) -> Result<Json<Value>, ApiError> {
    skald.approval().update_rule(p.id, body).await?;
    Ok(Json(json!({ "ok": true })))
}

// ── DELETE /api/approval/rules/:id ────────────────────────────────────────────

pub async fn delete_rule(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<RulePath>,
) -> Result<Json<Value>, ApiError> {
    skald.approval().delete_rule(p.id).await?;
    Ok(Json(json!({ "ok": true })))
}

// ── POST /api/approval/pending/:request_id/resolve ───────────────────────────
//
// Resolve a pending approval by request_id, regardless of which session or
// source it belongs to.  Useful for Telegram sub-agent approvals when the
// Telegram keyboard is unavailable.

#[derive(Deserialize)]
pub struct ResolvePath { pub request_id: i64 }

#[derive(Deserialize)]
pub struct ResolveBody {
    /// "approve" (default) or "reject".
    #[serde(default = "default_action")]
    pub action: String,
    #[serde(default)]
    pub note: String,
}

fn default_action() -> String { "approve".to_string() }

pub async fn resolve_pending(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<ResolvePath>,
    Json(body): Json<ResolveBody>,
) -> Result<Json<Value>, ApiError> {
    if body.action == "reject" {
        // Pass the raw note; the waiting session builds the canonical message.
        skald.inbox().reject(p.request_id, body.note.clone()).await;
    } else {
        skald.inbox().approve(p.request_id).await;
    }
    Ok(Json(json!({ "ok": true, "request_id": p.request_id, "action": body.action })))
}

// ── GET /api/approval/pending ─────────────────────────────────────────────────
//
// Returns all currently-pending approval requests (all sessions).

pub async fn list_pending(
    State(skald): State<Arc<Skald>>,
) -> Json<Value> {
    let pending = skald.inbox().list_pending().await.approvals;
    Json(json!(pending))
}

// ── GET /api/approval/tools ───────────────────────────────────────────────────
//
// Returns all available tools (built-in + MCP) so the frontend can show a
// picker with names and descriptions when creating approval rules.

pub async fn list_tools(
    State(skald): State<Arc<Skald>>,
) -> Result<Json<AllTools>, ApiError> {
    let mut tools = skald.catalog().list_all();
    let server_rows = crate::core::db::mcp_servers::all(skald.db()).await?;
    tools.mcp_servers = server_rows.into_iter()
        .map(|r| (r.name, McpServerMeta { friendly_name: r.friendly_name, description: r.description }))
        .collect();

    // Merge dynamically-discovered tools (recorded by `ToolDiscovery` when they
    // were offered to the LLM) that the catalog does not already surface — the
    // interface/plugin/provider tools injected outside the `ToolRegistry`. This
    // is what makes them configurable in the Security-groups grid. Names already
    // known as built-in or MCP tools are deduped out; the rest are grouped under
    // the "dynamic" category.
    let discovered = crate::core::db::known_tools::all(skald.db()).await?;
    let existing: HashSet<&str> = tools.built_in.iter()
        .chain(tools.mcp.iter())
        .map(|t| t.name.as_str())
        .collect();
    let mut extra: Vec<ToolInfo> = discovered.into_iter()
        .filter(|k| !existing.contains(k.name.as_str()))
        .map(|k| ToolInfo {
            name:        k.name,
            description: k.description,
            source:      "built-in".into(),
            server:      None,
            category:    Some("dynamic".into()),
        })
        .collect();
    drop(existing);
    tools.built_in.append(&mut extra);
    tools.built_in.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(Json(tools))
}
