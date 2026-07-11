use std::sync::Arc;

use axum::{Json, extract::{Path, State}};
use serde::{Deserialize, Serialize};

use skald_core::db::users::UserSummary;
use skald_core::skald::Skald;

use super::ApiError;

// ── GET /api/users ───────────────────────────────────────────────────────────

pub async fn list(State(skald): State<Arc<Skald>>) -> Result<Json<Vec<UserSummary>>, ApiError> {
    let users = skald.users().list().await?;
    Ok(Json(users))
}

// ── POST /api/users ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateUserBody {
    pub username:    String,
    pub display_name: Option<String>,
    pub role_id:      String,
    pub password:     String,
    #[serde(default)]
    pub encrypted:    bool,
}

#[derive(Serialize)]
pub struct CreatedUser {
    pub id: String,
}

pub async fn create(
    State(skald): State<Arc<Skald>>,
    Json(body): Json<CreateUserBody>,
) -> Result<Json<CreatedUser>, ApiError> {
    let username = body.username.trim();
    if username.is_empty() {
        return Err(ApiError::bad_request("username must not be empty"));
    }
    if body.password.is_empty() {
        return Err(ApiError::bad_request("password must not be empty"));
    }
    let id = skald
        .users()
        .register_user(username, body.display_name.as_deref(), &body.role_id, Some(&body.password), body.encrypted)
        .await?;

    // Provision the user's container now (blueprint §6). Best-effort: a failure here
    // is not fatal to user creation — boot reconciliation will retry.
    if let Err(e) = skald.container().ensure(&id).await {
        tracing::warn!(user = %id, error = %e, "failed to provision user container (will retry at next boot)");
    }

    Ok(Json(CreatedUser { id }))
}

// ── PUT /api/users/:id ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct UpdateUserBody {
    pub username:     String,
    pub display_name: Option<String>,
    pub role_id:      String,
    #[serde(default)]
    pub active:       bool,
}

pub async fn update(
    State(skald): State<Arc<Skald>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateUserBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let username = body.username.trim();
    if username.is_empty() {
        return Err(ApiError::bad_request("username must not be empty"));
    }
    skald_core::db::users::update_profile(
        skald.db(),
        &id,
        username,
        body.display_name.as_deref(),
        &body.role_id,
    )
    .await?;

    // active is separate because it's a boolean flip
    skald_core::db::users::set_active(skald.db(), &id, body.active).await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── DELETE /api/users/:id ────────────────────────────────────────────────────

pub async fn delete(
    State(skald): State<Arc<Skald>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    skald.users().delete_user(&id).await?;
    // Tear down the user's container (best-effort; a missing one is fine).
    if let Err(e) = skald.container().remove(&id).await {
        tracing::warn!(user = %id, error = %e, "failed to remove user container");
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── POST /api/users/:id/password ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ResetPasswordBody {
    pub password: String,
}

pub async fn reset_password(
    State(skald): State<Arc<Skald>>,
    Path(id): Path<String>,
    Json(body): Json<ResetPasswordBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if body.password.is_empty() {
        return Err(ApiError::bad_request("password must not be empty"));
    }
    // change_password with no old password and a new one resets it. For
    // encrypted users the admin must supply the old password; this endpoint
    // is for cleartext users or for admin-initiated resets of cleartext
    // accounts. For encrypted users, the user should change their own password.
    skald.users().change_password(&id, None, Some(&body.password)).await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
