use std::sync::Arc;

use axum::{
    Json, Extension,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::{Value, json};

use skald_core::skald::Skald;
use super::{ApiError, guard::AuthUser, require_context};

// ── Tool Permission Groups ────────────────────────────────────────────────────

pub async fn list_groups(
    State(skald): State<Arc<Skald>>,
) -> Result<Json<Value>, ApiError> {
    let groups = skald.run_context_manager().list_groups().await?;
    Ok(Json(json!(groups)))
}

#[derive(Deserialize)]
pub struct GroupBody {
    pub id:          String,
    pub name:        String,
    pub description: Option<String>,
}

pub async fn create_group(
    State(skald): State<Arc<Skald>>,
    Json(body): Json<GroupBody>,
) -> Result<Json<Value>, ApiError> {
    skald.run_context_manager().create_group(&body.id, &body.name, body.description.as_deref()).await?;
    Ok(Json(json!({ "id": body.id })))
}

#[derive(Deserialize)]
pub struct GroupPath { pub id: String }

#[derive(Deserialize)]
pub struct GroupUpdateBody {
    pub name:        String,
    pub description: Option<String>,
}

pub async fn update_group(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<GroupPath>,
    Json(body): Json<GroupUpdateBody>,
) -> Result<Json<Value>, ApiError> {
    let found = skald.run_context_manager().update_group(&p.id, &body.name, body.description.as_deref()).await?;
    if !found {
        return Err(ApiError::not_found("permission group not found"));
    }
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_group(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<GroupPath>,
) -> Result<StatusCode, ApiError> {
    skald.run_context_manager().delete_group(&p.id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct DuplicateGroupBody {
    pub id:   String,
    pub name: String,
}

pub async fn duplicate_group(
    State(skald): State<Arc<Skald>>,
    Path(p): Path<GroupPath>,
    Json(body): Json<DuplicateGroupBody>,
) -> Result<Json<Value>, ApiError> {
    skald.run_context_manager().duplicate_group(&p.id, &body.id, &body.name).await?;
    Ok(Json(json!({ "id": body.id })))
}

// ── Session run_context assignment ────────────────────────────────────────────

#[derive(Deserialize)]
pub struct SessionPath { pub session_id: i64 }

/// POST body: the full RunContext object, or JSON `null` to clear the context.
pub async fn set_session_run_context(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p): Path<SessionPath>,
    Json(ctx): Json<Option<skald_core::run_context::RunContext>>,
) -> Result<Json<Value>, ApiError> {
    let uctx = require_context(&skald, &auth.user_id).await?;
    // The session row (and its live handler) live in the caller's own pool, so the
    // persist + live update both target the user's context. Run-context *definitions*
    // (roles) remain instance-wide; only the per-session value is owner data.
    skald_core::db::chat_sessions::set_run_context(
        &uctx.pool,
        p.session_id,
        ctx.as_ref().map(|c| c.to_db()).as_deref(),
    ).await?;

    if let Some(handler) = uctx.sessions.active_handler(p.session_id).await {
        handler.set_run_context(ctx).await;
    }

    Ok(Json(json!({ "ok": true })))
}
