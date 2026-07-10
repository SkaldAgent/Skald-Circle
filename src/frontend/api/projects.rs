use std::sync::Arc;

use axum::{
    Json, Extension,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use skald_core::db::project_tickets::ProjectTicket;
use skald_core::db::projects::Project;
use skald_core::db::{project_tickets, projects};
use skald_core::run_context::RunContext;
use skald_core::skald::Skald;
use super::{ApiError, guard::AuthUser, require_context};

/// Source-id prefix for a project's interactive chat session (e.g. `project-42`).
/// A hyphen (not `:`) is used so the id is URL-safe in `/api/{source}/messages`.
pub const PROJECT_SOURCE_PREFIX: &str = "project-";

/// Agent that drives interactive project-chat sessions.
const PROJECT_COORDINATOR_AGENT: &str = "project-coordinator";

// ── Request/Response types ────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ProjectResponse {
    pub id:          i64,
    pub name:        String,
    pub path:        String,
    pub description: String,
    pub run_context: Option<String>,
    pub created_at:  String,
    pub updated_at:  String,
}

impl From<Project> for ProjectResponse {
    fn from(p: Project) -> Self {
        Self {
            id: p.id, name: p.name, path: p.path,
            description: p.description,
            run_context: p.run_context, created_at: p.created_at, updated_at: p.updated_at,
        }
    }
}

#[derive(Deserialize)]
pub struct ProjectBody {
    pub name:           String,
    pub path:           String,
    pub description:    Option<String>,
    pub security_group: Option<String>,
}

impl ProjectBody {
    fn rc_json(&self) -> Option<String> {
        self.security_group.as_ref().map(|sg| {
            RunContext::with_security_group(Some(sg.clone())).to_db()
        })
    }
}

#[derive(Serialize)]
pub struct TicketResponse {
    pub id:           i64,
    pub project_id:   i64,
    pub title:        String,
    pub description:  String,
    pub status:       String,
    pub agent_id:     String,
    pub run_context:  Option<String>,
    pub job_id:       Option<i64>,
    pub session_id:   Option<i64>,
    pub result:       Option<String>,
    pub error:        Option<String>,
    pub created_at:   String,
    pub started_at:   Option<String>,
    pub completed_at: Option<String>,
}

impl From<ProjectTicket> for TicketResponse {
    fn from(t: ProjectTicket) -> Self {
        Self {
            id: t.id, project_id: t.project_id, title: t.title,
            description: t.description, status: t.status, agent_id: t.agent_id,
            run_context: t.run_context, job_id: t.job_id, session_id: t.session_id,
            result: t.result, error: t.error, created_at: t.created_at,
            started_at: t.started_at, completed_at: t.completed_at,
        }
    }
}

#[derive(Deserialize)]
pub struct TicketBody {
    pub title:          String,
    pub description:    Option<String>,
    pub agent_id:       Option<String>,
    pub security_group: Option<String>,
}

impl TicketBody {
    fn rc_json(&self) -> Option<String> {
        self.security_group.as_ref().map(|sg| {
            RunContext::with_security_group(Some(sg.clone())).to_db()
        })
    }
}

pub struct ProjectPath { pub id: i64 }
pub struct TicketPath  { pub id: i64, pub tid: i64 }

impl<'de> Deserialize<'de> for ProjectPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Inner { id: i64 }
        let inner = Inner::deserialize(d)?;
        Ok(Self { id: inner.id })
    }
}

impl<'de> Deserialize<'de> for TicketPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Inner { id: i64, tid: i64 }
        let inner = Inner::deserialize(d)?;
        Ok(Self { id: inner.id, tid: inner.tid })
    }
}

// ── Project handlers ──────────────────────────────────────────────────────────

pub async fn list(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<ProjectResponse>>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let items = projects::list(&ctx.pool).await?;
    Ok(Json(items.into_iter().map(Into::into).collect()))
}

pub async fn create(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<ProjectBody>,
) -> Result<(StatusCode, Json<ProjectResponse>), ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let rc_json = body.rc_json();
    let project = projects::create(
        &ctx.pool,
        &body.name,
        &body.path,
        body.description.as_deref().unwrap_or(""),
        rc_json.as_deref(),
    ).await?;
    Ok((StatusCode::CREATED, Json(project.into())))
}

pub async fn get_project(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<ProjectPath>,
) -> Result<Json<ProjectResponse>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let project = projects::get(&ctx.pool, p.id).await?
        .ok_or_else(|| ApiError::not_found(format!("project {} not found", p.id)))?;
    Ok(Json(project.into()))
}

pub async fn update(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<ProjectPath>,
    Json(body): Json<ProjectBody>,
) -> Result<Json<ProjectResponse>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let rc_json = body.rc_json();
    let found = projects::update(
        &ctx.pool, p.id,
        &body.name, &body.path,
        body.description.as_deref().unwrap_or(""),
        rc_json.as_deref(),
    ).await?;
    if !found {
        return Err(ApiError::not_found(format!("project {} not found", p.id)));
    }
    let project = projects::get(&ctx.pool, p.id).await?
        .ok_or_else(|| ApiError::not_found(format!("project {} not found", p.id)))?;
    Ok(Json(project.into()))
}

pub async fn delete(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<ProjectPath>,
) -> Result<StatusCode, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let found = projects::delete(&ctx.pool, p.id).await?;
    if found { Ok(StatusCode::NO_CONTENT) }
    else { Err(ApiError::not_found(format!("project {} not found", p.id))) }
}

// ── Ticket handlers ───────────────────────────────────────────────────────────

pub async fn list_tickets(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<ProjectPath>,
) -> Result<Json<Vec<TicketResponse>>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let tickets = project_tickets::list_for_project(&ctx.pool, p.id).await?;
    Ok(Json(tickets.into_iter().map(Into::into).collect()))
}

pub async fn create_ticket(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<ProjectPath>,
    Json(body): Json<TicketBody>,
) -> Result<(StatusCode, Json<TicketResponse>), ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let rc_json = body.rc_json();
    let agent_id = body.agent_id.as_deref().map(str::trim).filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("agent_id is required — pick a task agent for this ticket"))?;
    let ticket = project_tickets::create(
        &ctx.pool, p.id,
        &body.title,
        body.description.as_deref().unwrap_or(""),
        agent_id,
        rc_json.as_deref(),
    ).await?;
    projects::touch(&ctx.pool, p.id).await?;
    Ok((StatusCode::CREATED, Json(ticket.into())))
}

pub async fn delete_ticket(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(tp): Path<TicketPath>,
) -> Result<StatusCode, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let ticket = project_tickets::get(&ctx.pool, tp.tid).await?;
    let found = project_tickets::delete(&ctx.pool, tp.tid).await?;
    if found {
        if let Some(t) = ticket {
            projects::touch(&ctx.pool, t.project_id).await?;
        }
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found(format!("ticket {} not found", tp.tid)))
    }
}

pub async fn start_ticket(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(tp): Path<TicketPath>,
) -> Result<StatusCode, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let ticket  = project_tickets::get(&ctx.pool, tp.tid).await?
        .ok_or_else(|| ApiError::not_found(format!("ticket {} not found", tp.tid)))?;
    let project = projects::get(&ctx.pool, ticket.project_id).await?
        .ok_or_else(|| ApiError::not_found(format!("project {} not found", ticket.project_id)))?;

    let base: Option<RunContext> =
        ticket.run_context.as_deref().and_then(RunContext::from_db)
        .or_else(|| project.run_context.as_deref().and_then(RunContext::from_db));
    let rc = skald_core::projects::build_runtime_run_context(&project, base);

    let origin_ref = format!("PROJECT_TASK:{}", tp.tid);
    let rc_json    = rc.to_db();
    let job = ctx.cron.spawn_async_job(
        &ticket.title,
        &ticket.description,
        &ticket.description,
        &ticket.agent_id,
        Some(&rc_json),
        &origin_ref,
    )?;

    project_tickets::start(&ctx.pool, tp.tid, job.id).await?;
    projects::touch(&ctx.pool, ticket.project_id).await?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn reset_ticket(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(tp): Path<TicketPath>,
) -> Result<StatusCode, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let project_id = project_tickets::get(&ctx.pool, tp.tid).await?.map(|t| t.project_id);
    project_tickets::reset(&ctx.pool, tp.tid).await?;
    if let Some(pid) = project_id {
        projects::touch(&ctx.pool, pid).await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ── Project chat session ──────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SessionResponse {
    pub source:     String,
    pub session_id: i64,
}

/// Resolves which agent + `RunContext` a `source` should be provisioned with.
///
/// `project-{id}` → (`project-coordinator`, project runtime context); any other source
/// → (`main`, no context). This is the single place that maps a source to its
/// provisioning config, shared by session-open and session-reset so the two never
/// diverge.
pub async fn provisioning_for_source(
    pool:   &SqlitePool,
    source: &str,
) -> Result<(String, Option<RunContext>), ApiError> {
    let Some(id) = source
        .strip_prefix(PROJECT_SOURCE_PREFIX)
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Ok(("main".to_string(), None));
    };

    let project = projects::get(pool, id).await?
        .ok_or_else(|| ApiError::not_found(format!("project {id} not found")))?;
    let base = project.run_context.as_deref().and_then(RunContext::from_db);
    let rc = skald_core::projects::build_runtime_run_context(&project, base);
    Ok((PROJECT_COORDINATOR_AGENT.to_string(), Some(rc)))
}

/// POST /api/projects/{id}/session — open (or resume) the project's chat session.
/// Pre-creates the `project-{id}` source with the coordinator agent + project context
/// so the WebSocket finds the right session when the frontend connects.
pub async fn open_session(
    Path(p): Path<ProjectPath>,
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<SessionResponse>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let source = format!("{PROJECT_SOURCE_PREFIX}{}", p.id);
    let (agent, rc) = provisioning_for_source(&ctx.pool, &source).await?;
    let session_id = ctx.chat_hub
        .provision_session(&source, &agent, rc.as_ref(), false)
        .await?;
    Ok(Json(SessionResponse { source, session_id }))
}
