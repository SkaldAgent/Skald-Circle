use std::sync::Arc;

use axum::{Json, extract::{Path, State}};
use serde::Deserialize;

use skald_core::db::roles::{self, ADMIN_ROLE_ID, Role};
use skald_core::skald::Skald;

use super::ApiError;

pub async fn list(State(skald): State<Arc<Skald>>) -> Result<Json<Vec<Role>>, ApiError> {
    let roles = roles::list(skald.db()).await?;
    Ok(Json(roles))
}

#[derive(Deserialize)]
pub struct CreateBody {
    pub id:               String,
    pub label:            String,
    pub permission_group: String,
    pub attrs:            Option<String>,
}

pub async fn create(
    State(skald): State<Arc<Skald>>,
    Json(body): Json<CreateBody>,
) -> Result<Json<Role>, ApiError> {
    let id = body.id.trim();
    if id.is_empty() || id == ADMIN_ROLE_ID {
        return Err(ApiError::bad_request("invalid role id"));
    }
    if body.label.trim().is_empty() {
        return Err(ApiError::bad_request("label must not be empty"));
    }
    if body.permission_group.trim().is_empty() {
        return Err(ApiError::bad_request("permission group must not be empty"));
    }
    roles::insert(skald.db(), id, body.label.trim(), &body.permission_group, body.attrs.as_deref())
        .await?;
    // Seed the standard self-service Connector capabilities (§14): a new role can
    // register remote MCPs and activate vetted catalog scripts, but not add new
    // local scripts or manage the catalog (admin-only).
    skald_core::db::role_capabilities::seed_defaults(skald.db(), id).await?;
    let role = roles::get(skald.db(), id).await?.ok_or_else(|| ApiError::not_found("role not found after insert"))?;
    Ok(Json(role))
}

#[derive(Deserialize)]
pub struct UpdateBody {
    pub label:            String,
    pub permission_group: String,
    pub attrs:            Option<String>,
}

pub async fn update(
    State(skald): State<Arc<Skald>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateBody>,
) -> Result<Json<Role>, ApiError> {
    if id == ADMIN_ROLE_ID {
        return Err(ApiError::bad_request("the built-in admin role cannot be modified"));
    }
    if body.label.trim().is_empty() {
        return Err(ApiError::bad_request("label must not be empty"));
    }
    if body.permission_group.trim().is_empty() {
        return Err(ApiError::bad_request("permission group must not be empty"));
    }
    let ok = roles::update(skald.db(), &id, body.label.trim(), &body.permission_group, body.attrs.as_deref())
        .await?;
    if !ok {
        return Err(ApiError::not_found("role not found"));
    }
    let role = roles::get(skald.db(), &id).await?.ok_or_else(|| ApiError::not_found("role not found after update"))?;
    Ok(Json(role))
}

pub async fn delete(
    State(skald): State<Arc<Skald>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if id == ADMIN_ROLE_ID {
        return Err(ApiError::bad_request("the built-in admin role cannot be deleted"));
    }
    let count = roles::user_count(skald.db(), &id).await?;
    if count > 0 {
        return Err(ApiError::bad_request(format!("{count} user(s) still assigned to this role")));
    }
    roles::delete(skald.db(), &id).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
