use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json, Extension,
};
use serde::{Deserialize, Serialize};

use std::sync::Arc;
use skald_core::skald::Skald;
use super::{ApiError, guard::AuthUser, require_context};

const KEY: &str = "DEBUG_MODE";

#[derive(Serialize)]
pub struct DebugModeResponse {
    pub enabled: bool,
}

#[derive(Deserialize)]
pub struct DebugModeBody {
    pub enabled: bool,
}

pub async fn get_debug_mode(
    State(skald): State<Arc<Skald>>,
) -> Result<impl IntoResponse, ApiError> {
    let value = skald.config().get(KEY).await?;
    let enabled = value.as_deref() == Some("true");
    Ok(Json(DebugModeResponse { enabled }))
}

pub async fn set_debug_mode(
    State(skald): State<Arc<Skald>>,
    Json(body):   Json<DebugModeBody>,
) -> Result<impl IntoResponse, ApiError> {
    let value = if body.enabled { "true" } else { "false" };
    skald.config().set(KEY, value).await?;
    Ok(Json(DebugModeResponse { enabled: body.enabled }))
}

// ── LLM requests log ─────────────────────────────────────────────────────────

const PAGE_SIZE: i64 = 20;

#[derive(Deserialize)]
pub struct LlmRequestsQuery {
    pub from: Option<String>,
    pub to:   Option<String>,
    pub page: Option<i64>,
}

#[derive(Serialize)]
pub struct LlmRequestItem {
    pub id:                    i64,
    pub model_name:            String,
    pub created_at:            String,
    pub input_tokens:          Option<i64>,
    pub output_tokens:         Option<i64>,
    pub cache_read_tokens:     Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub duration_ms:           i64,
    pub error_text:            Option<String>,
}

#[derive(Serialize)]
pub struct LlmRequestsResponse {
    pub items:     Vec<LlmRequestItem>,
    pub total:     i64,
    pub page:      i64,
    pub page_size: i64,
}

pub async fn list_llm_requests(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Query(params): Query<LlmRequestsQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let page   = params.page.unwrap_or(1).max(1);
    let offset = (page - 1) * PAGE_SIZE;

    // Metadata-only query on system.db. No JOIN with chat_sessions (that table
    // lives in the per-user owner bucket). Filters by user_id so each user sees
    // only their own requests.
    let items = sqlx::query_as::<_, (i64, String, String, Option<i64>, Option<i64>, Option<i64>, Option<i64>, i64, Option<String>)>(
        "SELECT
             id,
             model_name,
             created_at,
             input_tokens,
             output_tokens,
             cache_read_tokens,
             cache_creation_tokens,
             duration_ms,
             error_text
         FROM llm_requests
         WHERE user_id = ?
           AND (? IS NULL OR created_at >= ?)
           AND (? IS NULL OR created_at <= ?)
         ORDER BY created_at DESC
         LIMIT ? OFFSET ?",
    )
    .bind(&auth.user_id)
    .bind(&params.from).bind(&params.from)
    .bind(&params.to).bind(&params.to)
    .bind(PAGE_SIZE)
    .bind(offset)
    .fetch_all(&**skald.db())
    .await?
    .into_iter()
    .map(|(id, model_name, created_at, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, duration_ms, error_text)| {
        LlmRequestItem { id, model_name, created_at, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, duration_ms, error_text }
    })
    .collect::<Vec<_>>();

    let total = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)
         FROM llm_requests
         WHERE user_id = ?
           AND (? IS NULL OR created_at >= ?)
           AND (? IS NULL OR created_at <= ?)",
    )
    .bind(&auth.user_id)
    .bind(&params.from).bind(&params.from)
    .bind(&params.to).bind(&params.to)
    .fetch_one(&**skald.db())
    .await?;

    Ok(Json(LlmRequestsResponse { items, total, page, page_size: PAGE_SIZE }))
}

// ── LLM request detail ────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct LlmRequestDetail {
    pub id:                    i64,
    pub request_id:            Option<String>,
    pub stack_id:              Option<i64>,
    pub model_name:            String,
    pub created_at:            String,
    pub input_tokens:          Option<i64>,
    pub output_tokens:         Option<i64>,
    pub cache_read_tokens:     Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub duration_ms:           i64,
    pub error_text:            Option<String>,
    // Payload fields — now live in llm_request_payloads in the user's own database.
    // Left as None for now; a future cross-pool lookup via request_id can fill them.
    pub request_json:          Option<String>,
    pub request_headers:       Option<String>,
    pub response_json:         Option<String>,
    pub response_headers:      Option<String>,
}

pub async fn get_llm_request(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, ApiError> {
    let row = sqlx::query_as::<_, (i64, Option<String>, Option<i64>, String, String, Option<i64>, Option<i64>, Option<i64>, Option<i64>, i64, Option<String>)>(
        "SELECT
             id,
             request_id,
             stack_id,
             model_name,
             created_at,
             input_tokens,
             output_tokens,
             cache_read_tokens,
             cache_creation_tokens,
             duration_ms,
             error_text
         FROM llm_requests
         WHERE id = ? AND user_id = ?",
    )
    .bind(id)
    .bind(&auth.user_id)
    .fetch_optional(&**skald.db())
    .await?;

    let Some((id, request_id, stack_id, model_name, created_at, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, duration_ms, error_text)) = row else {
        return Err(ApiError::not_found(format!("llm_request {id} not found")));
    };

    // Try to fetch the payload from the user's own database via request_id.
    let payload = if let Some(ref rid) = request_id {
        let ctx = require_context(&skald, &auth.user_id).await.ok();
        if let Some(ctx) = ctx {
            sqlx::query_as::<_, (Option<String>, Option<String>, Option<String>, Option<String>)>(
                "SELECT request_json, request_headers, response_json, response_headers
                 FROM llm_request_payloads WHERE request_id = ?",
            )
            .bind(rid)
            .fetch_optional(&*ctx.pool)
            .await
            .ok()
            .flatten()
        } else { None }
    } else { None };

    let (request_json, request_headers, response_json, response_headers) = payload.unwrap_or((None, None, None, None));

    Ok(Json(LlmRequestDetail {
        id, request_id, stack_id, model_name, created_at,
        input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
        duration_ms, error_text,
        request_json, request_headers, response_json, response_headers,
    }))
}
