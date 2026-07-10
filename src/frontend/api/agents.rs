use axum::{Json, extract::State, response::IntoResponse};
use serde::Serialize;

use crate::core::agents::AgentMeta;
use crate::core::llm::{LlmModelInfo, sort_models_for_agent};
use std::sync::Arc;
use crate::core::skald::Skald;

use super::ApiError;

pub async fn list(_: State<Arc<Skald>>) -> Result<Json<Vec<AgentMeta>>, ApiError> {
    let agents = crate::core::agents::discover()?;
    Ok(Json(agents))
}

#[derive(Serialize)]
pub struct AgentDetail {
    pub meta:   AgentMeta,
    pub prompt: String,
    pub models: Vec<LlmModelInfo>,
}

pub async fn get(
    State(skald): State<Arc<Skald>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<AgentDetail>, ApiError> {
    let meta   = crate::core::agents::load_meta(&id)?;
    let prompt = crate::core::agents::load_prompt(&id)?;
    let all    = skald.manager().llm_manager().list_models_info().await;
    let models = sort_models_for_agent(all, meta.scope.as_deref(), meta.strength);
    Ok(Json(AgentDetail { meta, prompt, models }))
}

/// Serve the agent's icon image file (e.g. icon.png) from `agents/{id}/<icon_path>`.
pub async fn icon(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let meta = crate::core::agents::load_meta(&id)?;
    let icon_path = meta.icon.ok_or_else(|| {
        ApiError::not_found(format!("Agent '{}' has no icon configured", id))
    })?;
    let full_path = format!("agents/{id}/{icon_path}");

    let data = tokio::fs::read(&full_path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError::not_found(format!("Icon file not found: {full_path}"))
        } else {
            ApiError::from(e)
        }
    })?;

    // Determine content type based on extension
    let content_type = if full_path.ends_with(".svg") {
        "image/svg+xml"
    } else if full_path.ends_with(".png") {
        "image/png"
    } else if full_path.ends_with(".jpg") || full_path.ends_with(".jpeg") {
        "image/jpeg"
    } else if full_path.ends_with(".webp") {
        "image/webp"
    } else {
        "application/octet-stream"
    };

    Ok((
        [("Content-Type", content_type)],
        data,
    ))
}
