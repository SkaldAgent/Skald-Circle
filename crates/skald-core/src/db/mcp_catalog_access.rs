//! Which users the admin has authorized to activate each per-user catalog
//! connector (the catalog twin of [`super::mcp_global_access`]).
//!
//! Registry junction table in `system.db`, deny-by-default: a user may see and
//! activate a `per_user` catalog entry only if a row grants it. Supersedes
//! `mcp_catalog.role_filter` as the access gate. Both FKs are registry→registry
//! (allowed), mirroring `mcp_global_access` / `shared_folder_members`.

use anyhow::Result;
use sqlx::SqlitePool;

// ── Reads ────────────────────────────────────────────────────────────────────

/// The catalog entry names a user is authorized to activate.
pub async fn catalog_names_for_user(pool: &SqlitePool, user_id: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT catalog_name FROM mcp_catalog_access WHERE user_id = ? ORDER BY catalog_name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

/// The ids of the users authorized to activate a given catalog entry.
pub async fn users_for_catalog(pool: &SqlitePool, catalog_name: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT user_id FROM mcp_catalog_access WHERE catalog_name = ? ORDER BY user_id",
    )
    .bind(catalog_name)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(u,)| u).collect())
}

pub async fn has_access(pool: &SqlitePool, catalog_name: &str, user_id: &str) -> Result<bool> {
    let row = sqlx::query_as::<_, (i64,)>(
        "SELECT 1 FROM mcp_catalog_access WHERE catalog_name = ? AND user_id = ?",
    )
    .bind(catalog_name)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

// ── Writes ───────────────────────────────────────────────────────────────────

/// Grants a user access to a catalog entry. Idempotent on the PK.
pub async fn grant(pool: &SqlitePool, catalog_name: &str, user_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO mcp_catalog_access (catalog_name, user_id) VALUES (?, ?)",
    )
    .bind(catalog_name)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn revoke(pool: &SqlitePool, catalog_name: &str, user_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM mcp_catalog_access WHERE catalog_name = ? AND user_id = ?")
        .bind(catalog_name)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Replaces a user's full catalog-access list in one shot (the Users-page form:
/// "which connectors may this person use"). Returns the set of names that were
/// **removed** by this write, so the caller can deactivate any that were live.
pub async fn set_for_user(
    pool: &SqlitePool,
    user_id: &str,
    catalog_names: &[String],
) -> Result<Vec<String>> {
    let before: std::collections::HashSet<String> =
        catalog_names_for_user(pool, user_id).await?.into_iter().collect();
    let after: std::collections::HashSet<String> =
        catalog_names.iter().cloned().collect();

    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM mcp_catalog_access WHERE user_id = ?")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    for name in &after {
        sqlx::query(
            "INSERT OR IGNORE INTO mcp_catalog_access (catalog_name, user_id) VALUES (?, ?)",
        )
        .bind(name)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    Ok(before.difference(&after).cloned().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A registry-schema database in a throwaway temp dir (mirrors the harness in
    /// `shared_folders::tests`). FK enforcement is on, so `users` + `mcp_catalog`
    /// rows must exist before a grant references them.
    async fn registry_pool(tag: &str) -> (SqlitePool, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("skald-catalogaccess-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::init_system_pool(&dir.join("system.db").to_string_lossy())
            .await
            .unwrap();
        for (id, name) in [("u1", "alice"), ("u2", "bob")] {
            sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES (?, ?, 'admin', 0)")
                .bind(id).bind(name).execute(&pool).await.unwrap();
        }
        for cat in ["gmail", "pokemon"] {
            sqlx::query("INSERT INTO mcp_catalog (name, scope, source) VALUES (?, 'per_user', 'remote')")
                .bind(cat).execute(&pool).await.unwrap();
        }
        (pool, dir)
    }

    #[tokio::test]
    async fn grant_is_per_user_and_deny_by_default() {
        let (pool, dir) = registry_pool("deny-default").await;

        // Nothing granted yet — deny by default.
        assert!(!has_access(&pool, "gmail", "u1").await.unwrap());

        grant(&pool, "gmail", "u1").await.unwrap();
        assert!(has_access(&pool, "gmail", "u1").await.unwrap());
        // The grant is per-user: bob is unaffected.
        assert!(!has_access(&pool, "gmail", "u2").await.unwrap());
        assert_eq!(catalog_names_for_user(&pool, "u1").await.unwrap(), vec!["gmail"]);
        assert_eq!(users_for_catalog(&pool, "gmail").await.unwrap(), vec!["u1"]);

        revoke(&pool, "gmail", "u1").await.unwrap();
        assert!(!has_access(&pool, "gmail", "u1").await.unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn set_for_user_replaces_and_reports_revoked() {
        let (pool, dir) = registry_pool("set-for-user").await;

        // Start with gmail granted.
        let removed = set_for_user(&pool, "u1", &["gmail".into()]).await.unwrap();
        assert!(removed.is_empty());
        assert!(has_access(&pool, "gmail", "u1").await.unwrap());

        // Swap to pokemon: gmail is the revoked one, pokemon the new grant.
        let removed = set_for_user(&pool, "u1", &["pokemon".into()]).await.unwrap();
        assert_eq!(removed, vec!["gmail"]);
        assert!(!has_access(&pool, "gmail", "u1").await.unwrap());
        assert!(has_access(&pool, "pokemon", "u1").await.unwrap());

        // Clearing all reports pokemon as revoked.
        let removed = set_for_user(&pool, "u1", &[]).await.unwrap();
        assert_eq!(removed, vec!["pokemon"]);
        assert!(catalog_names_for_user(&pool, "u1").await.unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
