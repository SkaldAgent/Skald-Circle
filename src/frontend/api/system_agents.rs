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

use skald_core::db::{system_agent_runs, system_agent_user_settings, users};
use skald_core::event_triage::EVENT_TRIAGE_AGENT;
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

// ── Per-user schedule (admin, from the Users page) ───────────────────────────
//
// Event triage only, deliberately, even though the table behind it is keyed by
// agent. It is the one agent whose cadence is a property of the *person* rather
// than of the instance: it fires on inbound events, so someone on a dozen
// mailing lists is triaged on nearly every tick while a quiet account is
// triaged once a day, from the same setting. The lints read a store that only
// its owner edits, and the review is pinned to an hour of the night — neither
// has a per-person version of that problem, and a field on a page is a question
// the admin then has to answer for everybody.

/// Minutes accepted for an override. The floor is the scheduler's own tick
/// floor — anything below it is a number the loop cannot honour and would only
/// mislead. The ceiling is a day, past which "every so often" has stopped being
/// triage.
const MIN_OVERRIDE_MINUTES: i64 = 1;
const MAX_OVERRIDE_MINUTES: i64 = 24 * 60;

/// `GET /api/users/{id}/event-triage` — that user's schedule for event triage.
///
/// `interval_minutes` is `null` when they have no override, which is the state
/// the form renders as "instance default" — never as the default's value, or
/// saving an untouched form would silently pin them to today's setting.
pub async fn user_event_triage_get(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(target):    Path<String>,
) -> Result<Json<Value>, ApiError> {
    caps::require_admin(&skald, &auth.user_id).await?;
    require_user(&skald, &target).await?;

    let override_secs =
        system_agent_user_settings::interval_secs(skald.db(), EVENT_TRIAGE_AGENT, &target).await?;

    Ok(Json(json!({
        "interval_minutes":         override_secs.map(|s| s / 60),
        "default_interval_minutes": instance_interval_minutes(&skald).await,
    })))
}

#[derive(Deserialize)]
pub struct UserEventTriageBody {
    /// `None` (or a missing field) clears the override and returns the user to
    /// the instance schedule.
    #[serde(default)]
    pub interval_minutes: Option<i64>,
}

/// `PUT /api/users/{id}/event-triage` — set or clear that user's override.
pub async fn user_event_triage_set(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(target):    Path<String>,
    Json(body):      Json<UserEventTriageBody>,
) -> Result<Json<Value>, ApiError> {
    caps::require_admin(&skald, &auth.user_id).await?;
    require_user(&skald, &target).await?;

    match body.interval_minutes {
        Some(minutes) => {
            if !(MIN_OVERRIDE_MINUTES..=MAX_OVERRIDE_MINUTES).contains(&minutes) {
                return Err(ApiError::bad_request(format!(
                    "the interval must be between {MIN_OVERRIDE_MINUTES} and \
                     {MAX_OVERRIDE_MINUTES} minutes"
                )));
            }
            system_agent_user_settings::set_interval_secs(
                skald.db(),
                EVENT_TRIAGE_AGENT,
                &target,
                minutes * 60,
            )
            .await?;
        }
        None => {
            system_agent_user_settings::clear(skald.db(), EVENT_TRIAGE_AGENT, &target).await?;
        }
    }

    // Nothing is pushed: the scheduler re-reads the interval on every tick, and
    // due-ness is measured from the user's own last attempt — so a change lands
    // on the next wake-up (at most the base tick away) with no event and no
    // subscriber. The bus is for reconciliation of live state; this is a number
    // read from the database each time it is needed.
    Ok(Json(json!({
        "interval_minutes":         body.interval_minutes,
        "default_interval_minutes": instance_interval_minutes(&skald).await,
    })))
}

/// The instance-wide event-triage interval, in whole minutes, for the form's
/// "default" label. Read from the agent itself rather than the config key, so
/// the fallback to `config.yml` is the same one the scheduler makes.
async fn instance_interval_minutes(skald: &Skald) -> i64 {
    match skald.system_agents().get(EVENT_TRIAGE_AGENT) {
        Some(agent) => (agent.interval_secs().await / 60).max(1) as i64,
        None        => 0,
    }
}

async fn require_user(skald: &Skald, user_id: &str) -> Result<(), ApiError> {
    users::get(skald.db(), user_id)
        .await?
        .ok_or_else(|| ApiError::not_found("no such user"))?;
    Ok(())
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
