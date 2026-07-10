use axum::{
    extract::{Path, State},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::Value;

use std::sync::Arc;
use skald_core::skald::Skald;
use super::ApiError;

pub async fn list(State(skald): State<Arc<Skald>>) -> Result<impl IntoResponse, ApiError> {
    let plugins = skald.plugin_manager().list().await?;
    Ok(Json(plugins))
}

#[derive(Deserialize)]
pub struct UpdateBody {
    pub enabled: bool,
    pub config:  Value,
}

pub async fn update(
    State(skald): State<Arc<Skald>>,
    Path(id):     Path<String>,
    Json(body):   Json<UpdateBody>,
) -> Result<impl IntoResponse, ApiError> {
    skald.plugin_manager().update_config(&id, body.enabled, body.config).await?;
    Ok(())
}
