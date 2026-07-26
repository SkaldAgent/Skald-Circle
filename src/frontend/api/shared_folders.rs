//! Shared on-disk folders management API (blueprint §6/§0.1).
//!
//! Admin-curated shared directories. The admin creates a folder, describes what it
//! holds (the description is injected into the agent's system context so it knows
//! what to store there and when to read it), and grants members read-only or
//! read-write access. Capability-gated on `MANAGE_SHARED_FOLDERS` — admin-only for
//! now, but a single `grant` opens it to any role (§0.1). Never agent-driven.
//!
//! The folder rows + membership live in the registry (`system.db`); the physical
//! directory `{WD}/shared/{name}` is created here so the bind-mount has a source
//! and the admin can drop files in immediately. Propagating a membership change
//! into a *running* container and into a logged-in user's fs view is **not** done
//! here: this module only announces `UserMountsChanged` on the system bus, and the
//! lifecycle reconciler (`skald::wiring`) reacts (blueprint §6).

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::Json;
use core_api::system_bus::SystemEvent;
use serde::{Deserialize, Serialize};

use skald_core::db::{role_capabilities, shared_folders, users};
use skald_core::skald::Skald;

use super::guard::AuthUser;
use super::ApiError;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Fails with 403 unless the caller's role may manage shared folders. `admin`
/// holds every capability by construction (`role_capabilities::has`).
async fn require_manage(skald: &Skald, user_id: &str) -> Result<(), ApiError> {
    let user = users::get(skald.db(), user_id)
        .await?
        .ok_or_else(|| ApiError::unauthorized("unknown user"))?;
    if role_capabilities::has(skald.db(), &user.role_id, role_capabilities::MANAGE_SHARED_FOLDERS)
        .await?
    {
        Ok(())
    } else {
        Err(ApiError::forbidden("your role cannot manage shared folders"))
    }
}

/// Creates `{WD}/shared/{name}` — the bind-mount source. `name` is already
/// validated as a single safe component, so it cannot escape the shared root.
fn create_shared_dir(name: &str) -> Result<(), ApiError> {
    let dir = std::env::current_dir()?
        .join(skald_core::container::SHARED_DIR)
        .join(name);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApiError::bad_request(format!("failed to create folder directory: {e}")))?;
    Ok(())
}

/// Announces that a user's mount topology changed. The lifecycle reconciler
/// recreates their container with the new mounts and, if they are logged in,
/// refreshes their fs view + per-user MCP in place (blueprint §6 remount).
///
/// Emitted **after** the membership row is committed, since the reconciler reads
/// the current rows. Fire-and-forget by contract: a Docker hiccup is the
/// reconciler's to log, never this endpoint's to surface — the state settles at
/// the user's next login/boot.
fn remount(skald: &Skald, user_id: &str) {
    skald.system_bus().send(SystemEvent::UserMountsChanged { user_id: user_id.to_string() });
}

// ── response / request types ──────────────────────────────────────────────────

#[derive(Serialize)]
pub struct MemberView {
    pub user_id:   String,
    pub can_write: bool,
}

/// A folder plus its membership. Member identities are just ids — the frontend
/// joins them against `/api/users`, which it already loads for the member picker.
#[derive(Serialize)]
pub struct FolderView {
    pub id:          i64,
    pub folder_name: String,
    pub description: String,
    pub created_at:  String,
    pub members:     Vec<MemberView>,
}

async fn folder_view(skald: &Skald, f: shared_folders::SharedFolder) -> Result<FolderView, ApiError> {
    let members = shared_folders::members(skald.db(), f.id)
        .await?
        .into_iter()
        .map(|m| MemberView { user_id: m.user_id, can_write: m.can_write })
        .collect();
    Ok(FolderView {
        id:          f.id,
        folder_name: f.folder_name,
        description: f.description,
        created_at:  f.created_at,
        members,
    })
}

// ── GET /api/shared-folders ───────────────────────────────────────────────────

pub async fn list(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<FolderView>>, ApiError> {
    require_manage(&skald, &auth.user_id).await?;
    let folders = shared_folders::list_all(skald.db()).await?;
    let mut out = Vec::with_capacity(folders.len());
    for f in folders {
        out.push(folder_view(&skald, f).await?);
    }
    Ok(Json(out))
}

// ── POST /api/shared-folders ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateBody {
    pub folder_name:      String,
    #[serde(default)]
    pub description:      String,
}

pub async fn create(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body):      Json<CreateBody>,
) -> Result<Json<FolderView>, ApiError> {
    require_manage(&skald, &auth.user_id).await?;
    let name = body.folder_name.trim();
    if !shared_folders::is_valid_folder_name(name) {
        return Err(ApiError::bad_request(
            "folder name must be a single path component (no '/', '\\', '.' or '..')",
        ));
    }
    if shared_folders::get_by_name(skald.db(), name).await?.is_some() {
        return Err(ApiError::bad_request(format!("a folder named '{name}' already exists")));
    }

    let id = shared_folders::create(skald.db(), name, body.description.trim()).await?;
    // The bind-mount needs a real directory to point at; make it now.
    create_shared_dir(name)?;

    let folder = shared_folders::get(skald.db(), id)
        .await?
        .ok_or_else(|| ApiError::not_found("folder vanished after creation"))?;
    Ok(Json(folder_view(&skald, folder).await?))
}

// ── PATCH /api/shared-folders/{id} — description only (no rename) ──────────────

#[derive(Deserialize)]
pub struct DescriptionBody {
    pub description: String,
}

pub async fn update_description(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id):        Path<i64>,
    Json(body):      Json<DescriptionBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_manage(&skald, &auth.user_id).await?;
    if shared_folders::get(skald.db(), id).await?.is_none() {
        return Err(ApiError::not_found("no such folder"));
    }
    shared_folders::set_description(skald.db(), id, body.description.trim()).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── DELETE /api/shared-folders/{id} ───────────────────────────────────────────

pub async fn delete(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id):        Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_manage(&skald, &auth.user_id).await?;
    // Capture the members before the cascade delete so each can be unmounted after.
    let members = shared_folders::members(skald.db(), id).await.unwrap_or_default();
    shared_folders::delete(skald.db(), id).await?;
    // The on-disk directory is deliberately left in place: unsharing a folder must
    // not destroy the files inside it. The admin removes them by hand if intended.
    for m in &members {
        remount(&skald, &m.user_id);
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── POST /api/shared-folders/{id}/members ── add or re-grant (RO/RW) ───────────

#[derive(Deserialize)]
pub struct MemberBody {
    pub user_id:   String,
    #[serde(default)]
    pub can_write: bool,
}

pub async fn add_member(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id):        Path<i64>,
    Json(body):      Json<MemberBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_manage(&skald, &auth.user_id).await?;
    if shared_folders::get(skald.db(), id).await?.is_none() {
        return Err(ApiError::not_found("no such folder"));
    }
    // Catch an unknown user id here for a clean 400 — the membership FK would
    // otherwise surface it as an opaque 500.
    if users::get(skald.db(), &body.user_id).await?.is_none() {
        return Err(ApiError::bad_request("no such user"));
    }
    shared_folders::add_member(skald.db(), id, &body.user_id, body.can_write).await?;
    // Mount the folder into (or re-grant RO/RW inside) the member's environment.
    remount(&skald, &body.user_id);
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── DELETE /api/shared-folders/{id}/members/{user_id} ─────────────────────────

pub async fn remove_member(
    State(skald):       State<Arc<Skald>>,
    Extension(auth):    Extension<AuthUser>,
    Path((id, user_id)): Path<(i64, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_manage(&skald, &auth.user_id).await?;
    shared_folders::remove_member(skald.db(), id, &user_id).await?;
    // Unmount the folder from the (former) member's environment.
    remount(&skald, &user_id);
    Ok(Json(serde_json::json!({ "ok": true })))
}
