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

/// The raw junction read: is there a grant row? This is the **roster** question —
/// what an admin ticked on somebody's page — and it is what the access-editing
/// surfaces must show. It is *not* the authorization question; use
/// [`effective_access`] for that.
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

/// The authorization decision: may this user activate/run this connector?
///
/// The admin role holds every connector implicitly, exactly as it holds every
/// plugin ([`super::plugin_access::effective_access`]) and every capability
/// ([`super::role_capabilities::has`]). That implicit hold is not a convenience —
/// [`super::access_defaults`] *depends* on it: it skips admins when seeding grants
/// ("they already hold every plugin and connector implicitly, so a row for them
/// would be noise"), so without a short-circuit here an admin ends up with no row
/// and no implicit access, and is denied their own connectors. That was the bug:
/// `available` listed a per-user connector to the admin (who holds
/// `mcp.manage_catalog`) while `activate` refused it — visible but unusable.
///
/// An unknown user id resolves to `false`; errors propagate, so callers fail
/// closed.
pub async fn effective_access(pool: &SqlitePool, catalog_name: &str, user_id: &str) -> Result<bool> {
    if super::users::is_admin(pool, user_id).await? {
        return Ok(true);
    }
    has_access(pool, catalog_name, user_id).await
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
        // A non-admin, for the effective-access tests: only `admin` is seeded.
        sqlx::query("INSERT INTO roles (id, label, permission_group) VALUES ('member', 'Member', 'default')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES ('m1', 'mallory', 'member', 0)")
            .execute(&pool).await.unwrap();
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

    #[tokio::test]
    async fn an_admin_is_authorized_without_a_grant_row() {
        // The regression this exists for: `access_defaults` deliberately writes no
        // grant rows for admins, on the stated grounds that they hold every
        // connector implicitly. Nothing implemented that here, so an admin was
        // listed a connector (they hold `mcp.manage_catalog`) and then refused when
        // they tried to activate it.
        let (pool, dir) = registry_pool("admin-implicit").await;

        assert!(!has_access(&pool, "gmail", "u1").await.unwrap(), "no row, by design");
        assert!(effective_access(&pool, "gmail", "u1").await.unwrap(), "but an admin holds it");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_member_still_needs_the_grant() {
        // The other half: the short-circuit must not have widened anything for
        // anyone else. Deny-by-default is unchanged for a non-admin.
        let (pool, dir) = registry_pool("member-denied").await;

        assert!(!effective_access(&pool, "gmail", "m1").await.unwrap());
        grant(&pool, "gmail", "m1").await.unwrap();
        assert!(effective_access(&pool, "gmail", "m1").await.unwrap());
        // And a connector they were not granted stays denied.
        assert!(!effective_access(&pool, "pokemon", "m1").await.unwrap());

        // An unknown user is nobody, not an admin.
        assert!(!effective_access(&pool, "gmail", "ghost").await.unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
