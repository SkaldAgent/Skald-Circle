use anyhow::Result;
use sqlx::SqlitePool;

/// Persist an MCP grant scoped to a specific stack frame (sub-agent).
/// Uses INSERT OR IGNORE so calling it multiple times is safe.
pub async fn grant(pool: &SqlitePool, stack_id: i64, mcp_name: &str) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO stack_mcp_grants (stack_id, mcp_name)
         VALUES (?, ?)",
    )
    .bind(stack_id)
    .bind(mcp_name)
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns the names of all MCP servers granted for this stack frame.
pub async fn list_for_stack(pool: &SqlitePool, stack_id: i64) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT mcp_name FROM stack_mcp_grants WHERE stack_id = ? ORDER BY granted_at",
    )
    .bind(stack_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(name,)| name).collect())
}

/// Removes all MCP grants for a stack frame. Called when the frame terminates.
pub async fn delete_for_stack(pool: &SqlitePool, stack_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM stack_mcp_grants WHERE stack_id = ?")
        .bind(stack_id)
        .execute(pool)
        .await?;
    Ok(())
}
