use anyhow::Result;
use sqlx::SqlitePool;

pub async fn upsert(pool: &SqlitePool, session_id: i64, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO session_scratchpad (session_id, key, value)
         VALUES (?, ?, ?)
         ON CONFLICT (session_id, key) DO UPDATE SET value = excluded.value"
    )
    .bind(session_id)
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn for_session(pool: &SqlitePool, session_id: i64) -> Result<Vec<(String, String)>> {
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT key, value FROM session_scratchpad WHERE session_id = ? ORDER BY key"
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
