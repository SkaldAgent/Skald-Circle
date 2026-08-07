//! Which users may see and configure each plugin.
//!
//! Registry junction table in `system.db` — opt-in access: a plugin with no
//! rows here is visible to admins only. Mirrors `mcp_global_access`, except
//! `plugin_id` is a bare TEXT (not a FK to `plugins.id`): plugin identity
//! comes from compiled registration and a `plugins` row exists only after
//! the first toggle, so a never-configured plugin must still be grantable.

use anyhow::Result;
use sqlx::SqlitePool;

// ── Reads ────────────────────────────────────────────────────────────────────

/// The ids of the plugins a user has been granted access to.
pub async fn plugin_ids_for_user(pool: &SqlitePool, user_id: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT plugin_id FROM plugin_access WHERE user_id = ? ORDER BY plugin_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(p,)| p).collect())
}

/// The ids of the users granted access to a given plugin.
pub async fn users_for_plugin(pool: &SqlitePool, plugin_id: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT user_id FROM plugin_access WHERE plugin_id = ? ORDER BY user_id",
    )
    .bind(plugin_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(u,)| u).collect())
}

pub async fn has_access(pool: &SqlitePool, plugin_id: &str, user_id: &str) -> Result<bool> {
    let row = sqlx::query_as::<_, (i64,)>(
        "SELECT 1 FROM plugin_access WHERE plugin_id = ? AND user_id = ?",
    )
    .bind(plugin_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

/// The effective runtime access decision for a channel adapter: the admin role
/// holds every plugin implicitly (mirroring the web `/plugins/mine` view),
/// otherwise the user must be granted in `plugin_access`. An unknown user id
/// resolves to `false`. Errors propagate — the caller fails closed.
pub async fn effective_access(pool: &SqlitePool, plugin_id: &str, user_id: &str) -> Result<bool> {
    if crate::db::users::is_admin(pool, user_id).await? {
        return Ok(true);
    }
    has_access(pool, plugin_id, user_id).await
}

// ── Writes ───────────────────────────────────────────────────────────────────

/// Grants a user access to a plugin. Idempotent on the PK.
pub async fn grant(pool: &SqlitePool, plugin_id: &str, user_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO plugin_access (plugin_id, user_id) VALUES (?, ?)",
    )
    .bind(plugin_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn revoke(pool: &SqlitePool, plugin_id: &str, user_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM plugin_access WHERE plugin_id = ? AND user_id = ?")
        .bind(plugin_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Replaces a user's full plugin-grant list in one shot — the Users-page form
/// ("which plugins may this person use"), the per-user twin of
/// [`super::mcp_catalog_access::set_for_user`].
///
/// A blanket replace is correct because the form is fed the **complete** set of
/// grantable plugins: every registered plugin except the binding-managed ones
/// (`Plugin::manages_own_access`), and those never read this table — their
/// access is their own pairing — so clearing a stale row for one is a no-op.
///
/// Nothing has to be pushed after this write: unlike an MCP grant, which gates
/// a runtime snapshotted at login, a plugin grant is re-read from here on every
/// request and every inbound channel message, so a revoke takes effect at once.
pub async fn set_for_user(pool: &SqlitePool, user_id: &str, plugin_ids: &[String]) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM plugin_access WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for plugin_id in plugin_ids {
        sqlx::query("INSERT OR IGNORE INTO plugin_access (plugin_id, user_id) VALUES (?, ?)")
            .bind(plugin_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Replaces the full access list for a plugin in one shot (the plugin-shaped
/// twin of [`set_for_user`]). No UI writes through this any more — "who may use
/// what" is edited on the user's page — but it is the honest inverse of the
/// read model and the cheapest way to set a plugin's audience from a test.
pub async fn set_access(pool: &SqlitePool, plugin_id: &str, user_ids: &[String]) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM plugin_access WHERE plugin_id = ?")
        .bind(plugin_id)
        .execute(&mut *tx)
        .await?;
    for user_id in user_ids {
        sqlx::query("INSERT OR IGNORE INTO plugin_access (plugin_id, user_id) VALUES (?, ?)")
            .bind(plugin_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
