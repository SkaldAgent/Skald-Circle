//! DB operations for the `llm_requests` table (metadata only).
//!
//! Every `chat_with_tools` call is logged here by the
//! [`crate::chatbot::logging::LoggingChatbotClient`] wrapper.
//! Payloads (request/response bodies + headers) live in `llm_request_payloads`
//! in the owner bucket (`{userid}.db`), correlated by `request_id`.
//! Rows are retained for `llm.request_log.retention_days` days (default 14).

use anyhow::Result;
use sqlx::SqlitePool;

pub mod cleanup;

// ── Row struct ────────────────────────────────────────────────────────────────

pub struct LlmRequestRow {
    pub request_id:            Option<String>,
    pub user_id:               Option<String>,
    pub session_id:            Option<i64>,
    pub stack_id:              Option<i64>,
    pub model_name:            String,
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
            request_id, user_id, session_id, stack_id, model_name,
            error_text, input_tokens, output_tokens, duration_ms,
            cache_read_tokens, cache_creation_tokens
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(&row.request_id)
    .bind(&row.user_id)
    .bind(row.session_id)
    .bind(row.stack_id)
    .bind(&row.model_name)
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
