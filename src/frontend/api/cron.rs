use axum::{
    Json, Extension,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;

use skald_core::db::{scheduled_jobs, job_runs};
use std::sync::Arc;
use skald_core::skald::Skald;
use super::{ApiError, guard::AuthUser, require_context};

#[derive(serde::Serialize)]
pub struct JobResponse {
    pub id:                 i64,
    pub title:              String,
    pub description:        String,
    pub cron:               String,
    pub prompt:             String,
    pub agent_id:           String,
    pub enabled:            bool,
    pub single_run:         bool,
    pub kind:               String,
    pub last_run_at:        Option<String>,
    pub next_run_at:        Option<String>,
    pub created_at:         String,
    pub run_context:        Option<String>,
    pub running_session_id: Option<i64>,
    pub running_since:      Option<String>,
}

pub async fn list(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<JobResponse>>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let jobs = scheduled_jobs::list(&ctx.pool).await?;
    Ok(Json(jobs.into_iter().map(|j| JobResponse {
        id:                 j.id,
        title:              j.title,
        description:        j.description,
        cron:               j.cron,
        prompt:             j.prompt,
        agent_id:           j.agent_id,
        enabled:            j.enabled,
        single_run:         j.single_run,
        kind:               j.kind,
        last_run_at:        j.last_run_at,
        next_run_at:        j.next_run_at,
        created_at:         j.created_at,
        run_context:        j.run_context,
        running_session_id: j.running_session_id,
        running_since:      j.running_since,
    }).collect()))
}

pub async fn delete_job(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<i64>,
) -> Result<(), ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let found = scheduled_jobs::delete(&ctx.pool, id).await?;
    if found { Ok(()) } else { Err(ApiError::not_found(format!("job {id} not found"))) }
}

pub async fn toggle(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<(), ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let enabled = body["enabled"]
        .as_bool()
        .ok_or_else(|| ApiError::bad_request("'enabled' boolean required"))?;
    let found = scheduled_jobs::set_enabled(&ctx.pool, id, enabled).await?;
    if found { Ok(()) } else { Err(ApiError::not_found(format!("job {id} not found"))) }
}

#[derive(Deserialize)]
pub struct SetRunContextBody {
    pub security_group: Option<String>,
}

pub async fn set_run_context(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<i64>,
    Json(body): Json<SetRunContextBody>,
) -> Result<(), ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    use skald_core::run_context::RunContext;
    let json = body.security_group.as_ref().map(|sg| {
        RunContext::with_security_group(Some(sg.clone())).to_db()
    });
    let found = scheduled_jobs::set_run_context(&ctx.pool, id, json.as_deref()).await?;
    if found { Ok(()) } else { Err(ApiError::not_found(format!("job {id} not found"))) }
}

#[derive(serde::Serialize)]
pub struct JobRunResponse {
    pub id:             i64,
    pub job_id:         i64,
    pub job_title:      Option<String>,
    pub agent_id:       Option<String>,
    pub kind:           Option<String>,
    pub session_id:     Option<i64>,
    pub started_at:     String,
    pub completed_at:   Option<String>,
    pub duration_ms:    Option<i64>,
    pub status:         String,
    pub final_response: Option<String>,
    pub error:          Option<String>,
    pub created_at:     String,
}

pub async fn kill_job(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let job = scheduled_jobs::get_by_id(&ctx.pool, id).await?
        .ok_or_else(|| ApiError::not_found(format!("job {id} not found")))?;
    let session_id = job.running_session_id
        .ok_or_else(|| ApiError::bad_request("job is not currently running"))?;
    // Direct cancel on the user's own session manager — no system bus fan-out.
    ctx.sessions.cancel_session(session_id).await;
    Ok(StatusCode::ACCEPTED)
}

pub async fn list_runs(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<JobRunResponse>>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let runs = job_runs::list_all(&ctx.pool, 200).await?;
    Ok(Json(runs.into_iter().map(|r| JobRunResponse {
        id:             r.id,
        job_id:         r.job_id,
        job_title:      r.job_title,
        agent_id:       r.agent_id,
        kind:           r.kind,
        session_id:     r.session_id,
        started_at:     r.started_at,
        completed_at:   r.completed_at,
        duration_ms:    r.duration_ms,
        status:         r.status,
        final_response: r.final_response,
        error:          r.error,
        created_at:     r.created_at,
    }).collect()))
}
