use axum::{
    Json, Extension,
    extract::{Path, State},
};
use serde::Deserialize;
use serde_json::{Value, json};

use skald_core::approval::NewApprovalRule;
use skald_core::db::approval_rules;
use skald_core::tool_catalog::{AllTools, McpServerMeta, ToolInfo};
use std::collections::HashSet;
use std::sync::Arc;
use skald_core::skald::Skald;

use super::{ApiError, guard::AuthUser, require_context};

// ── GET /api/approval/rules ───────────────────────────────────────────────────

pub async fn list_rules(
    State(skald): State<Arc<Skald>>,
) -> Result<Json<Value>, ApiError> {
    let rules = approval_rules::list(skald.db()).await?;
    Ok(Json(json!(rules)))
}

// ── POST /api/approval/rules ──────────────────────────────────────────────────

pub async fn create_rule(
    State(skald): State<Arc<Skald>>,
    Json(body): Json<NewApprovalRule>,
) -> Result<Json<Value>, ApiError> {
    let id = approval_rules::insert(skald.db(), body).await?;
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
    approval_rules::update(skald.db(), p.id, body).await?;
    Ok(Json(json!({ "ok": true })))
}

// ── DELETE /api/approval/rules/:id ────────────────────────────────────────────

pub async fn delete_rule(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<RulePath>,
) -> Result<Json<Value>, ApiError> {
    approval_rules::delete(skald.db(), p.id).await?;
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
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<ResolvePath>,
    Json(body): Json<ResolveBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    if body.action == "reject" {
        // Pass the raw note; the waiting session builds the canonical message.
        ctx.inbox.reject(p.request_id, body.note.clone()).await;
    } else {
        ctx.inbox.approve(p.request_id).await;
    }
    Ok(Json(json!({ "ok": true, "request_id": p.request_id, "action": body.action })))
}

// ── GET /api/approval/pending ─────────────────────────────────────────────────
//
// Returns all currently-pending approval requests (all sessions).

pub async fn list_pending(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let pending = ctx.inbox.list_pending().await.approvals;
    Ok(Json(json!(pending)))
}

// ── GET /api/approval/tools ───────────────────────────────────────────────────
//
// Returns all available tools (built-in + MCP) so the frontend can show a
// picker with names and descriptions when creating approval rules.
//
// The MCP half comes from three places, because no single one sees every
// connector on the box (§7 — two runtimes, and only one of them is shared):
//
//   * the instance `ToolCatalog`, which wraps the ownerless GLOBAL runtime;
//   * the caller's own PER-USER runtime, live in their container — the only
//     way a connector activated moments ago shows up before any model has been
//     offered it;
//   * `known_tools`, the registry-side record of every tool that has existed on
//     this box, which is what covers a connector belonging to a user who is not
//     logged in right now. Security groups are instance-wide config, so leaving
//     those out would make the grid describe only whoever happens to be online.
//
// An `mcp__<server>__<tool>` row from `known_tools` is routed to the MCP bucket
// under its own server rather than the flat "dynamic" category: the grid groups
// by server, and a connector's tools listed loose among the interface tools are
// findable only by someone who already knows their names.

pub async fn list_tools(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<AllTools>, ApiError> {
    let mut tools = skald.catalog().list_all();
    let server_rows = skald_core::db::mcp_global_servers::all(skald.db()).await?;
    tools.mcp_servers = server_rows.into_iter()
        .map(|r| (r.name, McpServerMeta { friendly_name: r.friendly_name, description: r.description }))
        .collect();

    // The caller's per-user runtime. Best-effort: a context that is gone means a
    // stale session, which is the login path's problem, not this listing's.
    if let Ok(ctx) = require_context(&skald, &auth.user_id).await {
        let seen: HashSet<String> = tools.mcp.iter().map(|t| t.name.clone()).collect();
        for t in ctx.user_mcp.tools() {
            let id = t.tool_id();
            if seen.contains(&id) { continue; }
            tools.mcp.push(ToolInfo {
                name:        id,
                description: t.description,
                source:      "mcp".into(),
                server:      Some(t.server_name),
                category:    None,
            });
        }
    }

    // Merge dynamically-discovered tools (recorded by `ToolDiscovery` when they
    // were offered to the LLM) that the catalog does not already surface — the
    // interface/plugin/provider tools injected outside the `ToolRegistry`, plus
    // the per-user MCP tools recorded at login. This is what makes them
    // configurable in the Security-groups grid. Names already known as built-in
    // or MCP tools are deduped out; an `mcp__*` name joins the MCP bucket, the
    // rest are grouped under the "dynamic" category.
    let discovered = skald_core::db::known_tools::all(skald.db()).await?;
    let existing: HashSet<String> = tools.built_in.iter()
        .chain(tools.mcp.iter())
        .map(|t| t.name.clone())
        .collect();
    let mut extra: Vec<ToolInfo> = Vec::new();
    for k in discovered {
        if existing.contains(&k.name) { continue; }
        match skald_core::mcp::parse_mcp_tool_name(&k.name) {
            Some((server, _)) => tools.mcp.push(ToolInfo {
                name:        k.name.clone(),
                description: k.description,
                source:      "mcp".into(),
                server:      Some(server.to_string()),
                category:    None,
            }),
            None => extra.push(ToolInfo {
                name:        k.name,
                description: k.description,
                source:      "built-in".into(),
                server:      None,
                category:    Some("dynamic".into()),
            }),
        }
    }
    drop(existing);
    tools.built_in.append(&mut extra);
    tools.built_in.sort_by(|a, b| a.name.cmp(&b.name));
    tools.mcp.sort_by(|a, b| a.name.cmp(&b.name));

    // Metadata for every server that is not a global one: the catalog entry it
    // was activated from. A self-registered remote has none and falls back to
    // its raw server id in the UI.
    let unnamed: Vec<String> = tools.mcp.iter()
        .filter_map(|t| t.server.clone())
        .filter(|s| !tools.mcp_servers.contains_key(s))
        .collect();
    for server in unnamed {
        if tools.mcp_servers.contains_key(&server) { continue; }
        if let Some(row) = skald_core::db::mcp_catalog::get_by_name(skald.db(), &server).await? {
            tools.mcp_servers.insert(
                server,
                McpServerMeta { friendly_name: row.friendly_name, description: row.description },
            );
        }
    }

    Ok(Json(tools))
}
