//! System agents — the background agents the instance runs on a user's behalf
//! (blueprint §13): event triage and the two memory lints.
//!
//! **This page has two audiences, and the split is the whole design.**
//!
//! The run history is *the caller's own*, with no admin override: a run
//! summarises what arrived in someone's inbox, so it is stored in their own
//! encrypted database and read back through `require_context`, exactly like
//! their sessions. There is deliberately no "all users" view — the admin sees
//! their own runs and nobody else's, the same promise the rest of the private
//! pool makes (§2/§3). That is why the page is visible to everyone.
//!
//! The *settings* are instance-wide and therefore admin-only. They live here
//! rather than on the Config page because an agent's schedule and its run log
//! answer the same question — "why did this not do anything last night?" — and
//! splitting them across two pages made the answer require both. [`list_agents`]
//! serves the config half only to an admin; a member gets the descriptions and
//! nothing else, and the write path is `PUT /api/config/{key}`, which gates
//! again on its own.

use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Query, State},
};
use serde::Deserialize;
use serde_json::{Value, json};

use skald_core::db::system_agent_runs;
use skald_core::skald::Skald;

use super::guard::AuthUser;
use super::{ApiError, caps, config, require_context};

/// `GET /api/system-agents` — the agents this instance runs, in pass order.
///
/// The list *is* the set of owned config sets: every system agent has one by
/// construction (`SystemAgent::config_set`), so there is no second registry to
/// keep in step with the scheduler.
pub async fn list_agents(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Value>, ApiError> {
    let admin = caps::is_admin(&skald, &auth.user_id).await?;

    let owned: Vec<&core_api::ConfigSet> = skald
        .config_properties()
        .iter()
        .filter(|s| s.owner.is_some())
        .collect();

    // Values and options are resolved only for an admin. A member gets no
    // settings at all rather than read-only ones: there is nothing on this page
    // they could do with them, and shipping them would leak the instance's
    // configuration to every session for the sake of a disabled form.
    let items: Vec<Value> = if admin {
        // `render_sets` preserves order, so zipping is safe.
        let rendered = config::render_sets(&skald, &owned).await?;
        owned
            .iter()
            .zip(rendered)
            .map(|(set, view)| {
                json!({
                    "id":          set.owner,
                    "name":        set.name,
                    "description": set.description,
                    "config":      view,
                })
            })
            .collect()
    } else {
        owned
            .iter()
            .map(|set| {
                json!({
                    "id":          set.owner,
                    "name":        set.name,
                    "description": set.description,
                    "config":      Value::Null,
                })
            })
            .collect()
    };

    Ok(Json(json!({ "items": items, "can_configure": admin })))
}

#[derive(Deserialize)]
pub struct ListRunsQuery {
    /// Narrow to one agent (`event-triage`). Omitted = every system agent.
    pub agent_id: Option<String>,
    #[serde(default = "default_page")]
    pub page:     i64,
    #[serde(default = "default_per_page")]
    pub per_page: i64,
}

fn default_page()     -> i64 { 1 }
fn default_per_page() -> i64 { 20 }

/// `GET /api/system-agents/runs` — the caller's own run history, newest first.
pub async fn list_runs(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Query(q):        Query<ListRunsQuery>,
) -> Result<Json<Value>, ApiError> {
    let ctx      = require_context(&skald, &auth.user_id).await?;
    let per_page = q.per_page.clamp(1, 100);
    let offset   = (q.page.max(1) - 1) * per_page;
    let agent    = q.agent_id.as_deref().filter(|a| !a.is_empty());

    let total = system_agent_runs::count(&ctx.pool, agent).await?;
    let rows  = system_agent_runs::list(&ctx.pool, agent, per_page, offset).await?;

    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id":           r.id,
                "agent_id":     r.agent_id,
                "session_id":   r.session_id,
                "started_at":   r.started_at,
                "completed_at": r.completed_at,
                "duration_ms":  r.duration_ms,
                "status":       r.status,
                // Parsed here rather than in the browser: the column is the agent's
                // own JSON, and the client should not have to know it is a string.
                "stats":        r.stats.as_deref()
                                   .and_then(|s| serde_json::from_str::<Value>(s).ok()),
                "error":        r.error,
            })
        })
        .collect();

    Ok(Json(json!({
        "items":    items,
        "total":    total,
        "page":     q.page.max(1),
        "per_page": per_page,
    })))
}
