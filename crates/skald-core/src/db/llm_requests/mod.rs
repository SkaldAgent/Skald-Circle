//! DB operations for the `llm_requests` table.
//!
//! Every `chat_with_tools` call is logged here by the
//! [`crate::chatbot::logging::LoggingChatbotClient`] wrapper.
//! Rows are retained for `llm.request_log.retention_days` days (default 14).

use anyhow::Result;
use sqlx::SqlitePool;

pub mod cleanup;

// ── Row struct ────────────────────────────────────────────────────────────────

pub struct LlmRequestRow {
    pub session_id:            Option<i64>,
    pub stack_id:              Option<i64>,
    pub model_name:            String,
    /// Full HTTP request body sent to the provider (compact JSON, no pretty-print).
    pub request_json:          String,
    /// HTTP request headers as a compact JSON object (api-key redacted).
    pub request_headers:       Option<String>,
    /// Full HTTP response body from the provider (compact JSON).
    pub response_json:         Option<String>,
    /// HTTP response headers as a compact JSON object.
    pub response_headers:      Option<String>,
    /// Error message when the HTTP call itself failed (no response available).
    pub error_text:            Option<String>,
    pub input_tokens:          Option<i64>,
    pub output_tokens:         Option<i64>,
    /// Wall-clock time of the full HTTP round-trip in milliseconds.
    pub duration_ms:           i64,
    /// Tokens served from the provider's prompt cache (already parsed by the client).
    pub cache_read_tokens:     Option<i64>,
    /// Tokens written into the provider's prompt cache (Anthropic only).
    pub cache_creation_tokens: Option<i64>,
}

// ── Writes ────────────────────────────────────────────────────────────────────

pub async fn insert(pool: &SqlitePool, row: LlmRequestRow) -> Result<i64> {
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO llm_requests (
            session_id, stack_id, model_name,
            request_json, request_headers,
            response_json, response_headers,
            error_text, input_tokens, output_tokens, duration_ms,
            cache_read_tokens, cache_creation_tokens
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(row.session_id)
    .bind(row.stack_id)
    .bind(&row.model_name)
    .bind(&row.request_json)
    .bind(&row.request_headers)
    .bind(&row.response_json)
    .bind(&row.response_headers)
    .bind(&row.error_text)
    .bind(row.input_tokens)
    .bind(row.output_tokens)
    .bind(row.duration_ms)
    .bind(row.cache_read_tokens)
    .bind(row.cache_creation_tokens)
    .fetch_one(pool)
    .await?;

    Ok(id)
}

// ── Maintenance ───────────────────────────────────────────────────────────────

/// Physically deletes rows older than `days` days. Returns rows affected.
pub async fn delete_old_rows(pool: &SqlitePool, days: u32) -> Result<u64> {
    let cutoff = format!("-{days} days");
    let n = sqlx::query("DELETE FROM llm_requests WHERE created_at < datetime('now', ?)")
        .bind(&cutoff)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n)
}

/// Nulls out `request_json` for rows older than `days` days. Returns rows affected.
pub async fn null_request_payload(pool: &SqlitePool, days: u32) -> Result<u64> {
    let cutoff = format!("-{days} days");
    let n = sqlx::query(
        "UPDATE llm_requests SET request_json = '' \
         WHERE request_json != '' AND created_at < datetime('now', ?)",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n)
}

/// Nulls out `response_json` for rows older than `days` days. Returns rows affected.
pub async fn null_response_payload(pool: &SqlitePool, days: u32) -> Result<u64> {
    let cutoff = format!("-{days} days");
    let n = sqlx::query(
        "UPDATE llm_requests SET response_json = NULL \
         WHERE response_json IS NOT NULL AND created_at < datetime('now', ?)",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n)
}

/// Nulls out both header columns for rows older than `days` days. Returns rows affected.
pub async fn null_headers(pool: &SqlitePool, days: u32) -> Result<u64> {
    let cutoff = format!("-{days} days");
    let n = sqlx::query(
        "UPDATE llm_requests \
         SET request_headers = NULL, response_headers = NULL \
         WHERE (request_headers IS NOT NULL OR response_headers IS NOT NULL) \
           AND created_at < datetime('now', ?)",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n)
}
