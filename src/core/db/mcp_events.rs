use anyhow::Result;
use sqlx::SqlitePool;

// ── Row type ──────────────────────────────────────────────────────────────────

pub struct McpEvent {
    pub id:           i64,
    pub source:       String,
    pub method:       String,
    pub payload:      String,   // raw JSON of the "params" field
    pub processed:    bool,
    pub processed_at: Option<String>,
    pub created_at:   String,
}

// ── Write ─────────────────────────────────────────────────────────────────────

/// Insert a new event (processed = false).
pub async fn insert(
    pool:    &SqlitePool,
    source:  &str,
    method:  &str,
    payload: &str,
) -> Result<i64> {
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO mcp_events (source, method, payload)
         VALUES (?, ?, ?)
         RETURNING id",
    )
    .bind(source)
    .bind(method)
    .bind(payload)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Mark a batch of events as processed (sets processed = 1, processed_at = now).
pub async fn mark_processed(pool: &SqlitePool, ids: &[i64]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    // Build a parameterised IN clause.
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "UPDATE mcp_events
         SET processed = 1, processed_at = datetime('now')
         WHERE id IN ({placeholders})"
    );
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
    for id in ids {
        q = q.bind(id);
    }
    q.execute(pool).await?;
    Ok(())
}

// ── Read ──────────────────────────────────────────────────────────────────────

/// Oldest N pending (unprocessed) events, ordered oldest-first.
/// Used by TicManager to fetch a bounded batch each tick.
pub async fn pending_limited(pool: &SqlitePool, limit: i64) -> Result<Vec<McpEvent>> {
    let rows = sqlx::query_as::<_, (i64, String, String, String, bool, Option<String>, String)>(
        "SELECT id, source, method, payload, processed, processed_at, created_at
         FROM mcp_events
         WHERE processed = 0
         ORDER BY created_at ASC
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(row_to_event).collect())
}

/// All pending (unprocessed) events, ordered oldest-first.
pub async fn pending(pool: &SqlitePool) -> Result<Vec<McpEvent>> {
    let rows = sqlx::query_as::<_, (i64, String, String, String, bool, Option<String>, String)>(
        "SELECT id, source, method, payload, processed, processed_at, created_at
         FROM mcp_events
         WHERE processed = 0
         ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(row_to_event).collect())
}

/// All events (both processed and pending), most-recent first. Useful for debug/audit.
pub async fn all_recent(pool: &SqlitePool, limit: i64) -> Result<Vec<McpEvent>> {
    let rows = sqlx::query_as::<_, (i64, String, String, String, bool, Option<String>, String)>(
        "SELECT id, source, method, payload, processed, processed_at, created_at
         FROM mcp_events
         ORDER BY created_at DESC
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(row_to_event).collect())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn row_to_event(
    (id, source, method, payload, processed, processed_at, created_at): (
        i64, String, String, String, bool, Option<String>, String,
    ),
) -> McpEvent {
    McpEvent { id, source, method, payload, processed, processed_at, created_at }
}
