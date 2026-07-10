use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;

use crate::core::db::{scheduled_jobs, job_runs};
use std::sync::Arc;
use crate::core::skald::Skald;
use super::ApiError;

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

pub async fn list(State(skald): State<Arc<Skald>>) -> Result<Json<Vec<JobResponse>>, ApiError> {
    let jobs = scheduled_jobs::list(skald.db()).await?;
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
    Path(id): Path<i64>,
    State(skald): State<Arc<Skald>>,
) -> Result<(), ApiError> {
    let found = scheduled_jobs::delete(skald.db(), id).await?;
    if found { Ok(()) } else { Err(ApiError::not_found(format!("job {id} not found"))) }
}

pub async fn toggle(
    Path(id): Path<i64>,
    State(skald): State<Arc<Skald>>,
    Json(body): Json<serde_json::Value>,
) -> Result<(), ApiError> {
    let enabled = body["enabled"]
        .as_bool()
        .ok_or_else(|| ApiError::bad_request("'enabled' boolean required"))?;
    let found = scheduled_jobs::set_enabled(skald.db(), id, enabled).await?;
    if found { Ok(()) } else { Err(ApiError::not_found(format!("job {id} not found"))) }
}

#[derive(Deserialize)]
pub struct SetRunContextBody {
    pub security_group: Option<String>,
}

pub async fn set_run_context(
    Path(id): Path<i64>,
    State(skald): State<Arc<Skald>>,
    Json(body): Json<SetRunContextBody>,
) -> Result<(), ApiError> {
    use crate::core::run_context::RunContext;
    let json = body.security_group.as_ref().map(|sg| {
        RunContext::with_security_group(Some(sg.clone())).to_db()
    });
    let found = scheduled_jobs::set_run_context(skald.db(), id, json.as_deref()).await?;
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
    Path(id): Path<i64>,
    State(skald): State<Arc<Skald>>,
) -> Result<StatusCode, ApiError> {
    let job = scheduled_jobs::get_by_id(skald.db(), id).await?
        .ok_or_else(|| ApiError::not_found(format!("job {id} not found")))?;
    let session_id = job.running_session_id
        .ok_or_else(|| ApiError::bad_request("job is not currently running"))?;
    skald.system_bus().send(core_api::system_bus::SystemEvent::SessionCancelled { session_id });
    Ok(StatusCode::ACCEPTED)
}

pub async fn list_runs(State(skald): State<Arc<Skald>>) -> Result<Json<Vec<JobRunResponse>>, ApiError> {
    let runs = job_runs::list_all(skald.db(), 200).await?;
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
