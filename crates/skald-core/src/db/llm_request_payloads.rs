//! DB operations for the `llm_request_payloads` table (owner bucket).
//!
//! Full request/response payloads + headers for each LLM call. Lives in
//! `{userid}.db` (encrypted), correlated with the metadata row in `system.db`
//! via `request_id`.

use anyhow::Result;
use sqlx::SqlitePool;

pub struct PayloadRow {
    pub request_json:     String,
    pub request_headers:  Option<String>,
    pub response_json:    Option<String>,
    pub response_headers: Option<String>,
    pub request_id:       String,
}

pub async fn insert(pool: &SqlitePool, row: PayloadRow) -> Result<()> {
    sqlx::query(
        "INSERT INTO llm_request_payloads
            (request_id, request_json, request_headers, response_json, response_headers)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&row.request_id)
    .bind(&row.request_json)
    .bind(&row.request_headers)
    .bind(&row.response_json)
    .bind(&row.response_headers)
    .execute(pool)
    .await?;
    Ok(())
}
