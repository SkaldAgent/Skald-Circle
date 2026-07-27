//! System agents — the background agents the instance runs on a user's behalf
//! (blueprint §13). Today that is TIC; the surface is written for more.
//!
//! **Scoped to the caller, with no admin override.** A run summarises what
//! arrived in someone's inbox, so it is stored in their own encrypted database
//! and read back through `require_context`, exactly like their sessions. There
//! is deliberately no "all users" view: the admin sees their own runs and
//! nobody else's, which is the same promise the rest of the private pool makes
//! (§2/§3).

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
use super::{ApiError, require_context};

#[derive(Deserialize)]
pub struct ListRunsQuery {
    /// Narrow to one agent (`tic`). Omitted = every system agent.
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
