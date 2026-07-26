use std::sync::Arc;

use axum::{Json, extract::{Path, State}};
use serde::Deserialize;

use skald_core::agents::{self, AgentType};
use skald_core::db::roles::{self, ADMIN_ROLE_ID, RoleAttrs, Role};
use skald_core::skald::Skald;

use super::ApiError;
use super::projects::PROJECT_COORDINATOR_AGENT;

pub async fn list(State(skald): State<Arc<Skald>>) -> Result<Json<Vec<Role>>, ApiError> {
    let roles = roles::list(skald.db()).await?;
    Ok(Json(roles))
}

/// If the role's `attrs.chat_agent` is set, it must name an existing **chat** agent
/// other than the source-bound `project-coordinator` (which needs a project run-context
/// and is never a sensible personal default). Empty/absent is fine — the resolver falls
/// back to `DEFAULT_CHAT_AGENT`. Shared by create + update so the two can't drift.
fn validate_chat_agent(attrs: &Option<String>) -> Result<(), ApiError> {
    let Some(agent) = RoleAttrs::from_opt(attrs).chat_agent.filter(|s| !s.trim().is_empty()) else {
        return Ok(());
    };
    if agent == PROJECT_COORDINATOR_AGENT {
        return Err(ApiError::bad_request(
            "project-coordinator is source-driven and cannot be a role's default assistant",
        ));
    }
    match agents::load_meta(&agent) {
        Ok(meta) if meta.agent_type == AgentType::Chat => Ok(()),
        Ok(_)  => Err(ApiError::bad_request(format!("agent '{agent}' is not a chat agent"))),
        Err(_) => Err(ApiError::bad_request(format!("unknown agent '{agent}'"))),
    }
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
    validate_chat_agent(&body.attrs)?;
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
    validate_chat_agent(&body.attrs)?;
    let ok = roles::update(skald.db(), &id, body.label.trim(), &body.permission_group, body.attrs.as_deref())
        .await?;
    if !ok {
        return Err(ApiError::not_found("role not found"));
    }
    let role = roles::get(skald.db(), &id).await?.ok_or_else(|| ApiError::not_found("role not found after update"))?;
    // The edit may have narrowed the role's group set. Push that onto the members who
    // are logged in right now — their open sessions hold the previously-selected group
    // in RAM and would otherwise keep using it. (`delete` needs no equivalent: it
    // refuses while any user is still assigned.)
    skald.revalidate_security_groups_for_role(&id).await;
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
