//! Plugin management API.
//!
//! Two audiences, mirroring the Connectors split:
//! - **Admin** (`plugin.manage` capability): enable/disable, instance-wide
//!   config, and the per-user access grants (`plugin_access`).
//! - **Any user**: sees the plugins granted to them (`/plugins/mine`, read by
//!   the plugins' own page fragments) and submits their own per-user config
//!   (`/{id}/my-config` — e.g. Telegram's pairing code from its sidebar page).

use axum::{
    extract::{Extension, Path, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use std::sync::Arc;

use skald_core::db::{role_capabilities, roles::ADMIN_ROLE_ID, users};
use skald_core::skald::Skald;

use super::caps::require_cap;
use super::guard::AuthUser;
use super::ApiError;

// ── Admin: enable/disable + instance-wide config ─────────────────────────────

pub async fn list(
    State(skald):      State<Arc<Skald>>,
    Extension(auth):   Extension<AuthUser>,
) -> Result<impl IntoResponse, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_PLUGINS).await?;
    let plugins = skald.plugin_manager().list().await?;
    Ok(Json(plugins))
}

#[derive(Deserialize)]
pub struct UpdateBody {
    pub enabled: bool,
    pub config:  Value,
}

pub async fn update(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id):        Path<String>,
    Json(body):      Json<UpdateBody>,
) -> Result<impl IntoResponse, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_PLUGINS).await?;
    skald.plugin_manager().update_config(&id, body.enabled, body.config).await?;
    Ok(())
}

// ── Admin: per-user access grants ─────────────────────────────────────────────

#[derive(Serialize)]
pub struct AccessEntry {
    pub user_id:   String,
    pub username:  String,
    pub role_id:   String,
    pub granted:   bool,
}

pub async fn get_access(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id):        Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_PLUGINS).await?;
    let granted: std::collections::HashSet<String> =
        skald.plugin_manager().list_grants(&id).await?.into_iter().collect();
    let entries: Vec<AccessEntry> = users::list(skald.db())
        .await?
        .iter()
        .map(|u| AccessEntry {
            granted:  granted.contains(&u.id),
            user_id:  u.id.clone(),
            username: u.username.clone(),
            role_id:  u.role_id.clone(),
        })
        .collect();
    Ok(Json(entries))
}

#[derive(Deserialize)]
pub struct SetAccessBody {
    pub user_ids: Vec<String>,
}

pub async fn set_access(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id):        Path<String>,
    Json(body):      Json<SetAccessBody>,
) -> Result<impl IntoResponse, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_PLUGINS).await?;
    skald.plugin_manager().set_grants(&id, &body.user_ids).await?;
    Ok(())
}

// ── User: my plugins + my per-user config ────────────────────────────────────

/// Whether the caller is on the built-in admin role (admins implicitly hold
/// access to every enabled plugin).
async fn is_admin(skald: &Skald, user_id: &str) -> Result<bool, ApiError> {
    let user = users::get(skald.db(), user_id).await?
        .ok_or_else(|| ApiError::unauthorized("unknown user"))?;
    Ok(user.role_id == ADMIN_ROLE_ID)
}

pub async fn mine(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<impl IntoResponse, ApiError> {
    let admin = is_admin(&skald, &auth.user_id).await?;
    let plugins = skald.plugin_manager().list_accessible(&auth.user_id, admin).await?;
    Ok(Json(plugins))
}

/// The plugin-contributed web pages visible to the caller (menu entries).
/// Admin sees everything; everyone else sees the non-`admin_only` pages of
/// enabled plugins they hold a `plugin_access` grant for.
pub async fn pages(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<impl IntoResponse, ApiError> {
    let admin = is_admin(&skald, &auth.user_id).await?;
    let pages = skald.plugin_manager().web_pages_for(&auth.user_id, admin).await?;
    Ok(Json(pages))
}

pub async fn update_my_config(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id):        Path<String>,
    Json(config):    Json<Value>,
) -> Result<impl IntoResponse, ApiError> {
    let admin = is_admin(&skald, &auth.user_id).await?;
    if !admin && !skald.plugin_manager().has_access(&id, &auth.user_id).await? {
        return Err(ApiError::forbidden("you have no access to this plugin"));
    }
    skald.plugin_manager()
        .update_user_config(&id, &auth.user_id, config)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(())
}
