use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;

use skald_core::image_generate::{ImageGenerateModelInfo, ImageGenerateModelRecord};
use std::sync::Arc;
use skald_core::skald::Skald;
use super::ApiError;

// ── GET /api/image-generate/models ───────────────────────────────────────────

pub async fn list_models(
    State(skald): State<Arc<Skald>>,
) -> Result<Json<Vec<ImageGenerateModelInfo>>, ApiError> {
    Ok(Json(skald.image_generator_manager().list_all_info().await))
}

// ── POST /api/image-generate/models ──────────────────────────────────────────

#[derive(Deserialize)]
pub struct ModelPayload {
    pub provider_id: i64,
    pub model_id:    String,
    pub name:        String,
    pub priority:    Option<i32>,
}

impl From<ModelPayload> for ImageGenerateModelRecord {
    fn from(p: ModelPayload) -> Self {
        ImageGenerateModelRecord {
            id:          0,
            provider_id: p.provider_id,
            model_id:    p.model_id.clone(),
            name:        if p.name.is_empty() { p.model_id } else { p.name },
            priority:    p.priority.unwrap_or(100),
        }
    }
}

pub async fn create_model(
    State(skald): State<Arc<Skald>>,
    Json(payload): Json<ModelPayload>,
) -> Result<StatusCode, ApiError> {
    skald.image_generator_manager().add_model(ImageGenerateModelRecord::from(payload)).await?;
    Ok(StatusCode::CREATED)
}

// ── GET /api/image-generate/models/{id} ──────────────────────────────────────

pub async fn get_model(
    State(skald): State<Arc<Skald>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<ImageGenerateModelRecord>, ApiError> {
    skald.image_generator_manager().get_model(id).await
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("image generate model {id} not found")))
}

// ── PUT /api/image-generate/models/{id} ──────────────────────────────────────

pub async fn update_model(
    State(skald): State<Arc<Skald>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(payload): Json<ModelPayload>,
) -> Result<StatusCode, ApiError> {
    skald.image_generator_manager().update_model(id, ImageGenerateModelRecord::from(payload)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── DELETE /api/image-generate/models/{id} ───────────────────────────────────

pub async fn delete_model(
    State(skald): State<Arc<Skald>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<StatusCode, ApiError> {
    skald.image_generator_manager().delete_model(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
