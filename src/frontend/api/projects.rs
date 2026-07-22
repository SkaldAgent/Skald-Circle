//! Projects management API (blueprint §5 memory note / §6).
//!
//! A project is a **shareable endeavour** over an on-disk folder: metadata + membership
//! live in the registry (`system.db`, not encrypted), the folder lives at
//! `{WD}/projects/{owner_userid}/{slug}` and is bind-mounted into each member's
//! container (agent-visible as `projects/{owner_username}/{slug}`). Unlike shared
//! folders, sharing is **self-service**: the owner and any write-member can invite
//! others and grant read/write. Each member keeps their own private chat about the
//! project (conversations stay in their encrypted per-user DB).

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Extension,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use skald_core::db::project_members::{ProjectAccess, ProjectMember};
use skald_core::db::projects::Project;
use skald_core::db::{project_members, projects, users};
use skald_core::run_context::RunContext;
use skald_core::skald::Skald;
use super::{ApiError, guard::AuthUser, require_context};

/// Source-id prefix for a project's interactive chat session (e.g. `project-42`).
/// A hyphen (not `:`) is used so the id is URL-safe in `/api/{source}/messages`.
pub const PROJECT_SOURCE_PREFIX: &str = "project-";

/// Agent that drives interactive project-chat sessions.
pub(crate) const PROJECT_COORDINATOR_AGENT: &str = "project-coordinator";

// ── Request/Response types ────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct MemberView {
    pub user_id:   String,
    pub can_write: bool,
}

impl From<ProjectMember> for MemberView {
    fn from(m: ProjectMember) -> Self {
        Self { user_id: m.user_id, can_write: m.can_write }
    }
}

/// A project's detail view: identity + owner + the caller's capability + members.
#[derive(Serialize)]
pub struct ProjectDetail {
    pub id:            i64,
    pub name:          String,
    pub slug:          String,
    pub description:   String,
    pub owner_user_id: String,
    pub owner_name:    String,
    pub is_owner:      bool,
    pub can_write:     bool,
    /// The agent path of the project folder (`projects/{owner_username}/{slug}`) —
    /// the explorer's root; round-trips through `/api/file*` endpoints.
    pub root_path:     String,
    pub created_at:    String,
    pub updated_at:    String,
    pub members:       Vec<MemberView>,
}

#[derive(Deserialize)]
pub struct ProjectBody {
    pub name:        String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct MemberBody {
    pub user_id:   String,
    #[serde(default)]
    pub can_write: bool,
}

pub struct ProjectPath { pub id: i64 }
pub struct MemberPath  { pub id: i64, pub user_id: String }

impl<'de> Deserialize<'de> for ProjectPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Inner { id: i64 }
        let inner = Inner::deserialize(d)?;
        Ok(Self { id: inner.id })
    }
}

impl<'de> Deserialize<'de> for MemberPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Inner { id: i64, user_id: String }
        let inner = Inner::deserialize(d)?;
        Ok(Self { id: inner.id, user_id: inner.user_id })
    }
}

// ── helpers ────────────────────────────────────────────────────────────────────

/// `display_name || username` for a user id, or the id itself when the row is gone.
async fn user_label(skald: &Skald, user_id: &str) -> String {
    match users::get(skald.db(), user_id).await {
        Ok(Some(u)) => u.display_name.filter(|s| !s.is_empty()).unwrap_or(u.username),
        _ => user_id.to_string(),
    }
}

/// Loads a project and the caller's capability on it. 404 when the project is gone,
/// 403 when the caller is not a member (reads require membership).
async fn require_member(
    skald:   &Skald,
    id:      i64,
    user_id: &str,
) -> Result<(Project, bool), ApiError> {
    let project = projects::get(skald.db(), id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("project {id} not found")))?;
    let can_write = project_members::capability_of(skald.db(), id, user_id)
        .await?
        .ok_or_else(|| ApiError::forbidden("you are not a member of this project"))?;
    Ok((project, can_write))
}

/// Authority to manage membership / edit a project: the owner, or any write-member
/// (the sharing model is self-service, not admin-gated).
fn require_manage(project: &Project, user_id: &str, caller_can_write: bool) -> Result<(), ApiError> {
    if project.owner_user_id == user_id || caller_can_write {
        Ok(())
    } else {
        Err(ApiError::forbidden("read-only members cannot modify or share this project"))
    }
}

/// The host directory backing a project (`{WD}/projects/{owner_userid}/{slug}`).
fn project_dir(owner_user_id: &str, slug: &str) -> Result<PathBuf, ApiError> {
    Ok(std::env::current_dir()?
        .join(skald_core::container::PROJECTS_DIR)
        .join(owner_user_id)
        .join(slug))
}

/// Recreate a user's container with the new mount set (best-effort — settles at their
/// next login/boot on failure). See [`Skald::refresh_user_mounts`].
async fn remount(skald: &Skald, user_id: &str) {
    if let Err(e) = skald.refresh_user_mounts(user_id).await {
        tracing::warn!(user = %user_id, error = %e,
            "project remount failed (settles at next login/boot)");
    }
}

async fn detail(skald: &Skald, project: Project, caller: &str, can_write: bool) -> Result<ProjectDetail, ApiError> {
    let members = project_members::members(skald.db(), project.id).await?;
    let owner_name = user_label(skald, &project.owner_user_id).await;
    // The agent path keys on the owner's *username* (the mount segment), which
    // `owner_name` may not be (it's `display_name || username`).
    let owner_username = match users::get(skald.db(), &project.owner_user_id).await {
        Ok(Some(u)) => u.username,
        _ => project.owner_user_id.clone(),
    };
    Ok(ProjectDetail {
        is_owner: project.owner_user_id == caller,
        owner_name,
        id: project.id,
        name: project.name,
        slug: project.slug.clone(),
        description: project.description,
        owner_user_id: project.owner_user_id,
        can_write,
        root_path: format!("projects/{owner_username}/{}", project.slug),
        created_at: project.created_at,
        updated_at: project.updated_at,
        members: members.into_iter().map(Into::into).collect(),
    })
}

// ── Project handlers ──────────────────────────────────────────────────────────

/// GET /api/projects — the caller's projects (owned + shared-with-them).
pub async fn list(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<ProjectAccess>>, ApiError> {
    let items = project_members::list_for_user(skald.db(), &auth.user_id).await?;
    Ok(Json(items))
}

/// POST /api/projects — create a project owned by the caller (a private project = one
/// member). The folder is created and the caller's container remounted so the agent
/// can reach it immediately.
pub async fn create(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<ProjectBody>,
) -> Result<(StatusCode, Json<ProjectDetail>), ApiError> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("project name is required"));
    }
    let base = projects::slugify(name);
    let slug = projects::unique_slug(skald.db(), &auth.user_id, &base).await?;

    let project = projects::create(
        skald.db(),
        &auth.user_id,
        name,
        &slug,
        body.description.as_deref().unwrap_or("").trim(),
        None,
    )
    .await?;
    // The owner is a write-member, so mounts are uniform (private = one member).
    project_members::add_member(skald.db(), project.id, &auth.user_id, true).await?;
    // Create the bind-mount source, then remount so the container sees it.
    std::fs::create_dir_all(project_dir(&auth.user_id, &slug)?)
        .map_err(|e| ApiError::bad_request(format!("failed to create project directory: {e}")))?;
    remount(&skald, &auth.user_id).await;

    let d = detail(&skald, project, &auth.user_id, true).await?;
    Ok((StatusCode::CREATED, Json(d)))
}

/// GET /api/projects/{id} — the project detail (members require membership to read).
pub async fn get_project(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<ProjectPath>,
) -> Result<Json<ProjectDetail>, ApiError> {
    let (project, can_write) = require_member(&skald, p.id, &auth.user_id).await?;
    Ok(Json(detail(&skald, project, &auth.user_id, can_write).await?))
}

/// PUT /api/projects/{id} — edit name/description (owner or write-member). The slug is
/// immutable (it backs the on-disk folder + every member's path).
pub async fn update(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<ProjectPath>,
    Json(body): Json<ProjectBody>,
) -> Result<Json<ProjectDetail>, ApiError> {
    let (project, can_write) = require_member(&skald, p.id, &auth.user_id).await?;
    require_manage(&project, &auth.user_id, can_write)?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("project name is required"));
    }
    projects::update(
        skald.db(),
        p.id,
        name,
        body.description.as_deref().unwrap_or("").trim(),
        project.run_context.as_deref(),
    )
    .await?;
    let project = projects::get(skald.db(), p.id)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("project {} not found", p.id)))?;
    Ok(Json(detail(&skald, project, &auth.user_id, can_write).await?))
}

/// DELETE /api/projects/{id} — only the owner may delete. Cascades the membership,
/// removes the folder, and remounts every former member.
pub async fn delete(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<ProjectPath>,
) -> Result<StatusCode, ApiError> {
    let (project, _) = require_member(&skald, p.id, &auth.user_id).await?;
    if project.owner_user_id != auth.user_id {
        return Err(ApiError::forbidden("only the project owner can delete it"));
    }
    // Snapshot members before the cascade so we can remount them afterwards.
    let members = project_members::members(skald.db(), p.id).await?;
    projects::delete(skald.db(), p.id).await?;
    // Best-effort: drop the folder; the DB row is already gone.
    let _ = std::fs::remove_dir_all(project_dir(&project.owner_user_id, &project.slug)?);
    for m in members {
        remount(&skald, &m.user_id).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

// ── Membership (sharing) handlers ──────────────────────────────────────────────

/// POST /api/projects/{id}/members — add (or re-grant) a member. Owner or write-member.
pub async fn add_member(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<ProjectPath>,
    Json(body): Json<MemberBody>,
) -> Result<Json<Vec<MemberView>>, ApiError> {
    let (project, can_write) = require_member(&skald, p.id, &auth.user_id).await?;
    require_manage(&project, &auth.user_id, can_write)?;
    // Turn an FK violation into a clean 400.
    if users::get(skald.db(), &body.user_id).await?.is_none() {
        return Err(ApiError::bad_request("no such user"));
    }
    project_members::add_member(skald.db(), p.id, &body.user_id, body.can_write).await?;
    projects::touch(skald.db(), p.id).await?;
    remount(&skald, &body.user_id).await;

    let members = project_members::members(skald.db(), p.id).await?;
    Ok(Json(members.into_iter().map(Into::into).collect()))
}

/// DELETE /api/projects/{id}/members/{user_id} — remove a member. Owner or write-member.
/// The owner cannot be removed (delete the project instead).
pub async fn remove_member(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(mp): Path<MemberPath>,
) -> Result<Json<Vec<MemberView>>, ApiError> {
    let (project, can_write) = require_member(&skald, mp.id, &auth.user_id).await?;
    require_manage(&project, &auth.user_id, can_write)?;
    if project.owner_user_id == mp.user_id {
        return Err(ApiError::bad_request("the owner cannot be removed; delete the project instead"));
    }
    project_members::remove_member(skald.db(), mp.id, &mp.user_id).await?;
    projects::touch(skald.db(), mp.id).await?;
    remount(&skald, &mp.user_id).await;

    let members = project_members::members(skald.db(), mp.id).await?;
    Ok(Json(members.into_iter().map(Into::into).collect()))
}

// ── Project chat session ──────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct SessionResponse {
    pub source:     String,
    pub session_id: i64,
}

/// Resolves which agent + `RunContext` a `source` should be provisioned with.
///
/// `project-{id}` → (`project-coordinator`, project runtime context) **iff** the caller
/// is a member (else 403); any other source → (`main`, no context). The single place
/// that maps a source to its provisioning config, shared by session-open and
/// session-reset so the two never diverge.
pub async fn provisioning_for_source(
    skald:   &Skald,
    user_id: &str,
    source:  &str,
) -> Result<(String, Option<RunContext>), ApiError> {
    let Some(id) = source
        .strip_prefix(PROJECT_SOURCE_PREFIX)
        .and_then(|s| s.parse::<i64>().ok())
    else {
        // Non-project source → the caller's role-assigned entry agent (same resolver
        // the per-user hub uses, so explicit-create and lazy-create never diverge).
        let agent = skald_core::db::roles::default_chat_agent_for_user(skald.db(), user_id).await;
        return Ok((agent, None));
    };

    let (project, _can_write) = require_member(skald, id, user_id).await?;
    let owner_username = match users::get(skald.db(), &project.owner_user_id).await? {
        Some(u) => u.username,
        None => return Err(ApiError::not_found("project owner no longer exists")),
    };

    // Build the member views for the system-prompt block: display name + username.
    // Display name falls back to the username when unset; if the user row is gone
    // (shouldn't happen — FK — but be defensive), the username is the raw id.
    let member_rows = project_members::members(skald.db(), project.id).await?;
    let mut members: Vec<skald_core::projects::ProjectMemberView> = Vec::with_capacity(member_rows.len());
    for m in member_rows {
        let (display_name, username) = match users::get(skald.db(), &m.user_id).await? {
            Some(u) => (
                u.display_name.filter(|s| !s.is_empty()).unwrap_or_else(|| u.username.clone()),
                u.username,
            ),
            None => (m.user_id.clone(), m.user_id),
        };
        members.push(skald_core::projects::ProjectMemberView { display_name, username });
    }

    let base = project.run_context.as_deref().and_then(RunContext::from_db);
    let rc = skald_core::projects::build_project_run_context(&project, &owner_username, &members, base);
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
    let (agent, rc) = provisioning_for_source(&skald, &auth.user_id, &source).await?;
    let session_id = ctx.chat_hub
        .provision_session(&source, &agent, rc.as_ref(), false)
        .await?;
    Ok(Json(SessionResponse { source, session_id }))
}
