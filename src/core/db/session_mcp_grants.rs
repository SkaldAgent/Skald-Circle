use anyhow::Result;
use sqlx::SqlitePool;

/// Grant access to an MCP server for a session.
/// Uses INSERT OR IGNORE so calling it multiple times is safe.
pub async fn grant(pool: &SqlitePool, session_id: i64, mcp_name: &str) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO session_mcp_grants (session_id, mcp_name)
         VALUES (?, ?)"
    )
    .bind(session_id)
    .bind(mcp_name)
    .execute(pool)
    .await?;
    Ok(())
}

/// Revoke all MCP grants for a session.
pub async fn revoke_all(pool: &SqlitePool, session_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM session_mcp_grants WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Returns the names of all MCP servers granted for this session.
pub async fn list_for_session(pool: &SqlitePool, session_id: i64) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT mcp_name FROM session_mcp_grants WHERE session_id = ? ORDER BY granted_at"
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}
