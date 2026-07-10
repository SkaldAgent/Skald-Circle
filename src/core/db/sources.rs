use sqlx::SqlitePool;

pub struct Source {
    pub id:                String,
    pub active_session_id: Option<i64>,
    pub updated_at:        String,
}

/// Upsert a source, setting its active session.
pub async fn upsert(pool: &SqlitePool, id: &str, session_id: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO sources (id, active_session_id, updated_at)
         VALUES (?, ?, datetime('now'))
         ON CONFLICT(id) DO UPDATE SET
             active_session_id = excluded.active_session_id,
             updated_at        = excluded.updated_at",
    )
    .bind(id)
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Find a source by id.
pub async fn find(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<Source>> {
    let row = sqlx::query_as::<_, (String, Option<i64>, String)>(
        "SELECT id, active_session_id, updated_at FROM sources WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(id, active_session_id, updated_at)| Source { id, active_session_id, updated_at }))
}

/// Returns the active session id for a source, if set.
pub async fn active_session_id(pool: &SqlitePool, id: &str) -> anyhow::Result<Option<i64>> {
    let row = sqlx::query_as::<_, (Option<i64>,)>(
        "SELECT active_session_id FROM sources WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.and_then(|(sid,)| sid))
}
