use std::sync::Arc;

use axum::{
    Json, Extension,
    extract::{Path, State},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use skald_core::db::roles;
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

// ── GET /api/my/security-groups — the caller's selectable groups ──────────────

#[derive(Serialize)]
pub struct MySecurityGroup {
    pub id:         String,
    pub name:       String,
    pub is_default: bool,
}

/// The security-groups the calling user may pick in the chat picker: its role's
/// effective set joined with the group names (`admin` → every group). The composer
/// renders this like the model list; the server still enforces the set on write.
pub async fn my_security_groups(
    State(skald):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<MySecurityGroup>>, ApiError> {
    let user = skald
        .users()
        .get(&auth.user_id)
        .await?
        .ok_or_else(|| ApiError::not_found("user not found"))?;

    let all = skald.run_context_manager().list_groups().await?;

    let (allowed, default_id): (Vec<String>, String) = if user.role_id == roles::ADMIN_ROLE_ID {
        (all.iter().map(|g| g.id.clone()).collect(), "default".to_string())
    } else {
        match roles::get(skald.db(), &user.role_id).await? {
            Some(role) => (role.effective_groups(), role.permission_group.clone()),
            None => (vec!["default".to_string()], "default".to_string()),
        }
    };

    // Keep only ids that still exist as groups; carry the display name from there.
    let out = allowed
        .into_iter()
        .filter_map(|id| {
            all.iter().find(|g| g.id == id).map(|g| MySecurityGroup {
                is_default: g.id == default_id,
                id:         g.id.clone(),
                name:       g.name.clone(),
            })
        })
        .collect();

    Ok(Json(out))
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

    // Gate the requested context by the caller's role: a non-admin may only pick a
    // security-group in its role's set, and every other RunContext field is dropped
    // (fs-escalation hardening). admin passes through. Same validator the WS path uses.
    let user = skald
        .users()
        .get(&auth.user_id)
        .await?
        .ok_or_else(|| ApiError::not_found("user not found"))?;
    let ctx = match skald_core::run_context::validate_run_context_for_role(
        skald.db(),
        &user.role_id,
        ctx,
    )
    .await?
    {
        skald_core::run_context::RunContextDecision::Apply(c) => c,
        skald_core::run_context::RunContextDecision::Forbidden(g) => {
            return Err(ApiError::forbidden(format!(
                "security group '{g}' is not allowed for your role"
            )));
        }
    };

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
