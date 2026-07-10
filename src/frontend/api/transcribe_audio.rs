use axum::{
    Json,
    extract::{Multipart, State},
    http::StatusCode,
};
use serde::Serialize;

use std::sync::Arc;
use skald_core::skald::Skald;
use super::ApiError;

#[derive(Serialize)]
pub struct TranscribeResponse {
    pub text: String,
}

/// POST /api/transcribe/audio
///
/// Accepts a multipart/form-data body with a single field `audio` containing
/// the raw audio bytes. The `Content-Type` of the part determines the format
/// passed to the transcriber (e.g. `audio/webm` → `"webm"`).
pub async fn transcribe_audio(
    State(skald): State<Arc<Skald>>,
    mut multipart: Multipart,
) -> Result<Json<TranscribeResponse>, ApiError> {
    let transcriber = skald.transcribe_manager().get().await
        .ok_or_else(|| ApiError::bad_request("no transcription model configured"))?;

    let mut audio_bytes: Option<Vec<u8>> = None;
    let mut format = "webm".to_string();

    while let Some(field) = multipart.next_field().await
        .map_err(|e| ApiError::bad_request(format!("multipart error: {e}")))?
    {
        if field.name() == Some("audio") {
            // Derive format from content-type header of the part, e.g. "audio/webm" → "webm"
            if let Some(ct) = field.content_type() {
                if let Some(ext) = ct.split('/').nth(1) {
                    // Strip codec suffix: "webm;codecs=opus" → "webm"
                    format = ext.split(';').next().unwrap_or("webm").to_string();
                }
            }
            audio_bytes = Some(
                field.bytes().await
                    .map_err(|e| ApiError::bad_request(format!("failed to read audio: {e}")))?
                    .to_vec(),
            );
        }
    }

    let audio = audio_bytes
        .ok_or_else(|| ApiError::bad_request("missing 'audio' field in multipart body"))?;

    if audio.is_empty() {
        return Err(ApiError::bad_request("audio field is empty"));
    }

    let text = transcriber.transcribe(audio, &format).await
        .map_err(|e| {
            tracing::warn!(error = %e, "transcription failed");
            ApiError::from(e)
        })?;

    Ok(Json(TranscribeResponse { text }))
}

pub async fn has_transcribe(
    State(skald): State<Arc<Skald>>,
) -> StatusCode {
    if skald.transcribe_manager().get().await.is_some() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}
