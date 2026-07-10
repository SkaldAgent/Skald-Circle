use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use crate::core::db::project_tickets::ProjectTicket;
use crate::core::db::projects::Project;
use crate::core::run_context::RunContext;
use crate::core::skald::Skald;
use super::ApiError;

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
) -> Result<Json<Vec<ProjectResponse>>, ApiError> {
    let projects = skald.projects().list().await?;
    Ok(Json(projects.into_iter().map(Into::into).collect()))
}

pub async fn create(
    State(skald): State<Arc<Skald>>,
    Json(body): Json<ProjectBody>,
) -> Result<(StatusCode, Json<ProjectResponse>), ApiError> {
    let rc_json = body.rc_json();
    let rc = rc_json.as_deref().and_then(RunContext::from_db);
    let project = skald.projects().create(
        &body.name,
        &body.path,
        body.description.as_deref().unwrap_or(""),
        rc.as_ref(),
    ).await?;
    Ok((StatusCode::CREATED, Json(project.into())))
}

pub async fn get_project(
    Path(p): Path<ProjectPath>,
    State(skald): State<Arc<Skald>>,
) -> Result<Json<ProjectResponse>, ApiError> {
    let project = skald.projects().get(p.id).await?
        .ok_or_else(|| ApiError::not_found(format!("project {} not found", p.id)))?;
    Ok(Json(project.into()))
}

pub async fn update(
    Path(p): Path<ProjectPath>,
    State(skald): State<Arc<Skald>>,
    Json(body): Json<ProjectBody>,
) -> Result<Json<ProjectResponse>, ApiError> {
    let rc_json = body.rc_json();
    let rc = rc_json.as_deref().and_then(RunContext::from_db);
    let found = skald.projects().update(
        p.id,
        &body.name,
        &body.path,
        body.description.as_deref().unwrap_or(""),
        rc.as_ref(),
    ).await?;
    if !found {
        return Err(ApiError::not_found(format!("project {} not found", p.id)));
    }
    let project = skald.projects().get(p.id).await?
        .ok_or_else(|| ApiError::not_found(format!("project {} not found", p.id)))?;
    Ok(Json(project.into()))
}

pub async fn delete(
    Path(p): Path<ProjectPath>,
    State(skald): State<Arc<Skald>>,
) -> Result<StatusCode, ApiError> {
    let found = skald.projects().delete(p.id).await?;
    if found { Ok(StatusCode::NO_CONTENT) }
    else { Err(ApiError::not_found(format!("project {} not found", p.id))) }
}

// ── Ticket handlers ───────────────────────────────────────────────────────────

pub async fn list_tickets(
    Path(p): Path<ProjectPath>,
    State(skald): State<Arc<Skald>>,
) -> Result<Json<Vec<TicketResponse>>, ApiError> {
    let tickets = skald.ticket_manager().list(p.id).await?;
    Ok(Json(tickets.into_iter().map(Into::into).collect()))
}

pub async fn create_ticket(
    Path(p): Path<ProjectPath>,
    State(skald): State<Arc<Skald>>,
    Json(body): Json<TicketBody>,
) -> Result<(StatusCode, Json<TicketResponse>), ApiError> {
    let rc_json = body.rc_json();
    let rc = rc_json.as_deref().and_then(RunContext::from_db);
    // Tickets run a task sub-agent — no default. The agent's `type == task` is enforced
    // when the ticket starts, via TaskManager::spawn_async_job (require_task_agent).
    let agent_id = body.agent_id.as_deref().map(str::trim).filter(|s| !s.is_empty())
        .ok_or_else(|| ApiError::bad_request("agent_id is required — pick a task agent for this ticket"))?;
    let ticket = skald.ticket_manager().create(
        p.id,
        &body.title,
        body.description.as_deref().unwrap_or(""),
        agent_id,
        rc.as_ref(),
    ).await?;
    Ok((StatusCode::CREATED, Json(ticket.into())))
}

pub async fn delete_ticket(
    Path(tp): Path<TicketPath>,
    State(skald): State<Arc<Skald>>,
) -> Result<StatusCode, ApiError> {
    let found = skald.ticket_manager().delete(tp.tid).await?;
    if found { Ok(StatusCode::NO_CONTENT) }
    else { Err(ApiError::not_found(format!("ticket {} not found", tp.tid))) }
}

pub async fn start_ticket(
    Path(tp): Path<TicketPath>,
    State(skald): State<Arc<Skald>>,
) -> Result<StatusCode, ApiError> {
    skald.ticket_manager().start(tp.tid).await?;
    Ok(StatusCode::ACCEPTED)
}

pub async fn reset_ticket(
    Path(tp): Path<TicketPath>,
    State(skald): State<Arc<Skald>>,
) -> Result<StatusCode, ApiError> {
    skald.ticket_manager().reset(tp.tid).await?;
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
    skald:  &Skald,
    source: &str,
) -> Result<(String, Option<RunContext>), ApiError> {
    let Some(id) = source
        .strip_prefix(PROJECT_SOURCE_PREFIX)
        .and_then(|s| s.parse::<i64>().ok())
    else {
        return Ok(("main".to_string(), None));
    };

    let project = skald.projects().get(id).await?
        .ok_or_else(|| ApiError::not_found(format!("project {id} not found")))?;
    let base = project.run_context.as_deref().and_then(RunContext::from_db);
    let rc = crate::core::projects::build_runtime_run_context(&project, base);
    Ok((PROJECT_COORDINATOR_AGENT.to_string(), Some(rc)))
}

/// POST /api/projects/{id}/session — open (or resume) the project's chat session.
/// Pre-creates the `project-{id}` source with the coordinator agent + project context
/// so the WebSocket finds the right session when the frontend connects.
pub async fn open_session(
    Path(p): Path<ProjectPath>,
    State(skald): State<Arc<Skald>>,
) -> Result<Json<SessionResponse>, ApiError> {
    let source = format!("{PROJECT_SOURCE_PREFIX}{}", p.id);
    let (agent, rc) = provisioning_for_source(&skald, &source).await?;
    let session_id = skald.chat_hub()
        .provision_session(&source, &agent, rc.as_ref(), false)
        .await?;
    Ok(Json(SessionResponse { source, session_id }))
}
