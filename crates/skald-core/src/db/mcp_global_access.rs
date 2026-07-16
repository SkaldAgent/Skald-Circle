//! Which users may use each globally-active MCP connector (blueprint §15).
//!
//! Registry junction table in `system.db` — the per-user access filter over
//! `mcp_global_servers`. Both FKs are registry→registry (allowed), mirroring
//! `shared_folder_members`. The admin UI's "grant to all / by role" is just a
//! convenience that inserts rows here.

use anyhow::Result;
use sqlx::SqlitePool;

// ── Reads ────────────────────────────────────────────────────────────────────

/// The names of the **enabled** global servers a user may use. Feeds the
/// `accessible_global` snapshot captured when the user's context is built.
pub async fn server_names_for_user(pool: &SqlitePool, user_id: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT s.name
         FROM   mcp_global_access a
         JOIN   mcp_global_servers s ON s.id = a.server_id
         WHERE  a.user_id = ? AND s.enabled = 1
         ORDER  BY s.name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

/// The ids of the users granted access to a given global server.
pub async fn users_for_server(pool: &SqlitePool, server_id: i64) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT user_id FROM mcp_global_access WHERE server_id = ? ORDER BY user_id",
    )
    .bind(server_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(u,)| u).collect())
}

pub async fn has_access(pool: &SqlitePool, server_id: i64, user_id: &str) -> Result<bool> {
    let row = sqlx::query_as::<_, (i64,)>(
        "SELECT 1 FROM mcp_global_access WHERE server_id = ? AND user_id = ?",
    )
    .bind(server_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

// ── Writes ───────────────────────────────────────────────────────────────────

/// Grants a user access to a global server. Idempotent on the PK.
pub async fn grant(pool: &SqlitePool, server_id: i64, user_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO mcp_global_access (server_id, user_id) VALUES (?, ?)",
    )
    .bind(server_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn revoke(pool: &SqlitePool, server_id: i64, user_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM mcp_global_access WHERE server_id = ? AND user_id = ?")
        .bind(server_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Replaces the full access list for a server in one shot (used by the admin UI's
/// "set who can use this" form, incl. the by-role bulk grant).
pub async fn set_access(pool: &SqlitePool, server_id: i64, user_ids: &[String]) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM mcp_global_access WHERE server_id = ?")
        .bind(server_id)
        .execute(&mut *tx)
        .await?;
    for user_id in user_ids {
        sqlx::query("INSERT OR IGNORE INTO mcp_global_access (server_id, user_id) VALUES (?, ?)")
            .bind(server_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
