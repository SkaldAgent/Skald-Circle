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
    extract::{Path, Query, State},
};
use serde::Deserialize;
use serde_json::{Value, json};

use skald_core::db::system_agent_runs;
use skald_core::skald::Skald;
use skald_core::system_agents::{AgentScope, ManualRun, ManualRunError};

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

    // Whether this agent's tab gets a "Run now" button. Read from the agent's own
    // scope rather than a list of ids here, so a future agent is classified by
    // what it is: a pass about somebody else has no "for me" to run.
    let can_run_now = |set: &core_api::ConfigSet| {
        set.owner
            .as_deref()
            .and_then(|id| skald.system_agents().get(id))
            .is_some_and(|a| a.scope() != AgentScope::PerSubject)
    };

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
                    "can_run_now": can_run_now(set),
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
                    "can_run_now": can_run_now(set),
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

/// `POST /api/system-agents/{agent_id}/run` — run this agent **now**, for the caller.
///
/// **Not admin-gated, and that is the same decision the run log makes.** A pass
/// runs in the caller's own runtime over their own data and reports to them
/// alone, so there is nothing here an admin should have to approve — the person
/// who would read the report is the person asking for it. What stays the admin's
/// is the *schedule* and the on/off switch, which this endpoint does not touch:
/// a disabled agent answers `409` rather than running once for whoever asked.
///
/// Returns as soon as the pass is scheduled; `nothing_to_do` is the honest answer
/// for a store with nothing in it, and leaves no run row behind — exactly what an
/// idle scheduled pass does.
pub async fn run_now(
    State(skald):      State<Arc<Skald>>,
    Extension(auth):   Extension<AuthUser>,
    Path(agent_id):    Path<String>,
) -> Result<Json<Value>, ApiError> {
    match skald.run_system_agent_now(&agent_id, &auth.user_id).await {
        Ok(ManualRun::Started)     => Ok(Json(json!({ "status": "started" }))),
        Ok(ManualRun::NothingToDo) => Ok(Json(json!({ "status": "nothing_to_do" }))),
        Err(e) => Err(match e {
            ManualRunError::UnknownAgent   => ApiError::not_found(e.to_string()),
            ManualRunError::Unsupported    => ApiError::bad_request(e.to_string()),
            ManualRunError::Disabled       => ApiError::conflict(e.to_string()),
            ManualRunError::AlreadyRunning => ApiError::conflict(e.to_string()),
            ManualRunError::Locked         => ApiError::unauthorized(e.to_string()),
            ManualRunError::Failed(err)    => ApiError::from(err),
        }),
    }
}
