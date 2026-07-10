use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;

use skald_core::transcribe::{RemoteTranscribeModelInfo, TranscribeModelInfo, TranscribeModelRecord};
use std::sync::Arc;
use skald_core::skald::Skald;
use super::ApiError;

// ── GET /api/transcribe/models ────────────────────────────────────────────────

pub async fn list_models(
    State(skald): State<Arc<Skald>>,
) -> Result<Json<Vec<TranscribeModelInfo>>, ApiError> {
    Ok(Json(skald.transcribe_manager().list_all_info().await))
}

// ── POST /api/transcribe/models ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ModelPayload {
    pub provider_id: i64,
    pub model_id:    String,
    pub name:        String,
    pub language:    Option<String>,
    pub priority:    Option<i32>,
}

impl From<ModelPayload> for TranscribeModelRecord {
    fn from(p: ModelPayload) -> Self {
        TranscribeModelRecord {
            id:          0, // assigned by DB
            provider_id: p.provider_id,
            model_id:    p.model_id.clone(),
            name:        if p.name.is_empty() { p.model_id } else { p.name },
            language:    p.language,
            priority:    p.priority.unwrap_or(100),
        }
    }
}

pub async fn create_model(
    State(skald): State<Arc<Skald>>,
    Json(payload): Json<ModelPayload>,
) -> Result<StatusCode, ApiError> {
    skald.transcribe_manager().add_model(TranscribeModelRecord::from(payload)).await?;
    Ok(StatusCode::CREATED)
}

// ── GET /api/transcribe/models/{id} ──────────────────────────────────────────

pub async fn get_model(
    State(skald): State<Arc<Skald>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<TranscribeModelRecord>, ApiError> {
    skald.transcribe_manager().get_model(id).await
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("transcribe model {id} not found")))
}

// ── PUT /api/transcribe/models/{id} ──────────────────────────────────────────

pub async fn update_model(
    State(skald): State<Arc<Skald>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(payload): Json<ModelPayload>,
) -> Result<StatusCode, ApiError> {
    skald.transcribe_manager().update_model(id, TranscribeModelRecord::from(payload)).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ── GET /api/transcribe/providers/{id}/models ─────────────────────────────────

pub async fn provider_models(
    State(skald): State<Arc<Skald>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<Json<Vec<RemoteTranscribeModelInfo>>, ApiError> {
    let models = skald.transcribe_manager().list_provider_models(id).await?;
    Ok(Json(models))
}

// ── DELETE /api/transcribe/models/{id} ───────────────────────────────────────

pub async fn delete_model(
    State(skald): State<Arc<Skald>>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<StatusCode, ApiError> {
    skald.transcribe_manager().delete_model(id).await?;
    Ok(StatusCode::NO_CONTENT)
}
