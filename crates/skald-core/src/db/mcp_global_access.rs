//! Which users may use each globally-active MCP connector (blueprint §15).
//!
//! Registry junction table in `system.db` — the per-user access filter over
//! `mcp_global_servers`. Both FKs are registry→registry (allowed), mirroring
//! `shared_folder_members`. The admin UI's "grant to all / by role" is just a
//! convenience that inserts rows here.

use anyhow::Result;
use sqlx::SqlitePool;

// ── Reads ────────────────────────────────────────────────────────────────────

/// The names of the **enabled** global servers granted to a user by a row. The
/// roster read — for the runtime set, use [`effective_server_names_for_user`].
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

/// The enabled global servers a user may actually use. Feeds the
/// `accessible_global` snapshot captured when the user's context is built, and so
/// decides which shared MCP tools their agent is offered at all.
///
/// An admin gets every enabled server, because they are never given grant rows
/// (see [`effective_access`]). Without this an admin's session snapshotted an
/// empty set and simply had no shared connectors — the same root cause as being
/// refused activation, one layer down and much quieter, since nothing errors: the
/// tools are just absent.
pub async fn effective_server_names_for_user(
    pool: &SqlitePool,
    user_id: &str,
) -> Result<Vec<String>> {
    if !super::users::is_admin(pool, user_id).await? {
        return server_names_for_user(pool, user_id).await;
    }
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT name FROM mcp_global_servers WHERE enabled = 1 ORDER BY name",
    )
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

/// The raw junction read: is there a grant row? This is the **roster** question —
/// what an admin ticked on somebody's page — and it is what the access-editing
/// surfaces must show. For "may this user use it", use [`effective_access`].
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

/// The authorization decision: may this user use this shared connector?
///
/// Admins hold every connector implicitly — see
/// [`super::mcp_catalog_access::effective_access`] for why that short-circuit is
/// load-bearing rather than cosmetic (`access_defaults` skips seeding them rows
/// precisely because it is supposed to exist).
pub async fn effective_access(pool: &SqlitePool, server_id: i64, user_id: &str) -> Result<bool> {
    if super::users::is_admin(pool, user_id).await? {
        return Ok(true);
    }
    has_access(pool, server_id, user_id).await
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

/// Replaces one user's full global-access list in one shot — the per-user twin of
/// [`set_access`], for the Users-page "which connectors may this person use" form.
pub async fn set_for_user(pool: &SqlitePool, user_id: &str, server_ids: &[i64]) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM mcp_global_access WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for server_id in server_ids {
        sqlx::query("INSERT OR IGNORE INTO mcp_global_access (server_id, user_id) VALUES (?, ?)")
            .bind(server_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A registry-schema database with one admin, one member, and two global
    /// servers — one of them disabled, since "enabled" is part of the answer.
    async fn registry_pool(tag: &str) -> (SqlitePool, PathBuf, i64) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("skald-globalaccess-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::init_system_pool(&dir.join("system.db").to_string_lossy())
            .await
            .unwrap();

        sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES ('adm', 'adm', 'admin', 0)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO roles (id, label, permission_group) VALUES ('member', 'Member', 'default')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES ('mem', 'mem', 'member', 0)")
            .execute(&pool).await.unwrap();

        let sid = sqlx::query("INSERT INTO mcp_global_servers (name, enabled) VALUES ('websearch', 1)")
            .execute(&pool).await.unwrap().last_insert_rowid();
        sqlx::query("INSERT INTO mcp_global_servers (name, enabled) VALUES ('offline', 0)")
            .execute(&pool).await.unwrap();

        (pool, dir, sid)
    }

    #[tokio::test]
    async fn an_admin_holds_every_enabled_global_without_a_row() {
        let (pool, dir, sid) = registry_pool("admin-implicit").await;

        assert!(!has_access(&pool, sid, "adm").await.unwrap(), "no row, by design");
        assert!(effective_access(&pool, sid, "adm").await.unwrap());
        // The snapshot that decides which shared MCP tools the session is offered.
        // A disabled server is still excluded — implicit access is not a bypass of
        // the admin having switched something off.
        assert_eq!(
            effective_server_names_for_user(&pool, "adm").await.unwrap(),
            vec!["websearch"],
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_member_still_needs_the_grant() {
        let (pool, dir, sid) = registry_pool("member-denied").await;

        assert!(!effective_access(&pool, sid, "mem").await.unwrap());
        assert!(effective_server_names_for_user(&pool, "mem").await.unwrap().is_empty());

        grant(&pool, sid, "mem").await.unwrap();
        assert!(effective_access(&pool, sid, "mem").await.unwrap());
        assert_eq!(
            effective_server_names_for_user(&pool, "mem").await.unwrap(),
            vec!["websearch"],
        );

        // An unknown user is nobody, not an admin.
        assert!(!effective_access(&pool, sid, "ghost").await.unwrap());
        assert!(effective_server_names_for_user(&pool, "ghost").await.unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
