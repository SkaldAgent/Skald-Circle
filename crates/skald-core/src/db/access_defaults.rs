//! Default access: who a newly-installed plugin/connector reaches, and what a
//! newly-created user starts out holding.
//!
//! The three grant tables ([`plugin_access`], [`mcp_global_access`],
//! [`mcp_catalog_access`]) are **deny-by-default and stay that way**: a row means
//! access, its absence means none, and every read fails closed. What changed is
//! not the semantics of the tables but *who writes the rows and when* — the admin
//! no longer has to grant a plugin person by person after enabling it.
//!
//! ## Why the default is materialized rather than evaluated
//!
//! The tempting alternative is to leave the tables lazy and answer each check as
//! `COALESCE(grant.allowed, object.grant_by_default)`, with signed rows recording
//! exceptions. It needs no seeding, but it costs two things worth more:
//!
//! - **The checkbox loses a state.** With signed exceptions an unticked box on the
//!   user's page means either "denied" or "just following the default", and the
//!   admin cannot see which. Materialized, a tick is a row and nothing else.
//! - **"Who has what" stops being one query.** The gate, the plugin's roster and
//!   the user's checklist all read the same junction today; under lazy evaluation
//!   each would have to recompose default + exception, and `plugin_access.plugin_id`
//!   is bare TEXT with no `plugins` row to join against (see that module's header).
//!
//! So the default is applied at exactly **two moments**, and never again:
//!
//! | moment | what happens |
//! |---|---|
//! | an object is **created** (plugin first toggled, global connector enabled, catalog entry installed) | [`seed_new_object`] grants it to every auto-grant user |
//! | a user is **created** | [`seed_new_user`] grants them every default-on object |
//!
//! Deliberately *not* on enable/disable: re-enabling a plugin must not resurrect a
//! grant the admin took away, so the trigger is the row's birth, not its flag.
//!
//! ## Who counts as an auto-grant user
//!
//! The role decides, through `roles.attrs.auto_grant` (§0.1: an attribute, never a
//! hardcoded role id). It defaults to `true`, so the open behaviour needs no
//! configuration; the seeded `children` preset sets it to `false`, which is the
//! reason the attribute exists — an admin installing a connector at 11pm should not
//! be silently handing it to a minor. Admins are skipped: they already hold every
//! plugin and connector implicitly, so a row for them would be noise.
//!
//! Seeding **only ever adds** access. Nothing here can revoke, which is why it is
//! safe to run best-effort from a creation path (a failure means a missing
//! convenience grant, never an unintended one).

use anyhow::Result;
use sqlx::SqlitePool;

use super::{mcp_catalog_access, mcp_global_access, plugin_access, roles};

/// One grantable object, in whichever of the three junctions owns it.
#[derive(Debug, Clone, Copy)]
pub enum Grantable<'a> {
    /// A plugin id (`plugins.id`).
    Plugin(&'a str),
    /// A globally-active connector (`mcp_global_servers.id`).
    GlobalServer(i64),
    /// A `per_user` catalog entry, by name (`mcp_catalog.name`).
    Catalog(&'a str),
}

// ── Who ──────────────────────────────────────────────────────────────────────

/// Whether a role's members are auto-granted new objects. `admin` is `false`:
/// not a denial — admins hold everything implicitly, so seeding rows for them
/// would only add noise to every roster. An unknown role grants nothing.
pub async fn role_auto_grants(pool: &SqlitePool, role_id: &str) -> Result<bool> {
    if role_id == roles::ADMIN_ROLE_ID {
        return Ok(false);
    }
    match roles::get(pool, role_id).await? {
        Some(role) => Ok(role.attrs_parsed().auto_grant),
        None => Ok(false),
    }
}

/// The ids of the users a newly-created object is granted to.
///
/// Deactivated users are included: `active = 0` gates logging in, not what the
/// directory says a person may use, and skipping them would leave a hole the day
/// they are switched back on.
pub async fn auto_grant_user_ids(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows =
        sqlx::query_as::<_, (String, String)>("SELECT id, role_id FROM users ORDER BY id")
            .fetch_all(pool)
            .await?;

    // Resolve each distinct role once — a household has a handful of roles and
    // potentially many more users.
    let mut verdict: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for (user_id, role_id) in rows {
        let allowed = match verdict.get(&role_id) {
            Some(v) => *v,
            None => {
                let v = role_auto_grants(pool, &role_id).await?;
                verdict.insert(role_id.clone(), v);
                v
            }
        };
        if allowed {
            out.push(user_id);
        }
    }
    Ok(out)
}

// ── The two seeding moments ──────────────────────────────────────────────────

/// Grants a **newly-created** object to every auto-grant user. A no-op when the
/// object opts out of the default (`grant_by_default = 0`) or has vanished.
/// Returns how many grants were written.
///
/// Call it once, right after the row is inserted — never on a re-enable.
pub async fn seed_new_object(pool: &SqlitePool, target: Grantable<'_>) -> Result<usize> {
    if !object_grants_by_default(pool, target).await? {
        return Ok(0);
    }
    let users = auto_grant_user_ids(pool).await?;
    for user_id in &users {
        match target {
            Grantable::Plugin(id) => plugin_access::grant(pool, id, user_id).await?,
            Grantable::GlobalServer(id) => mcp_global_access::grant(pool, id, user_id).await?,
            Grantable::Catalog(name) => mcp_catalog_access::grant(pool, name, user_id).await?,
        }
    }
    Ok(users.len())
}

/// Grants a **newly-created** user every object that is on by default, so a new
/// member arrives with the same tools everyone else already has. A no-op for a
/// role that opts out (and for `admin`, who needs no rows). Returns how many
/// grants were written.
pub async fn seed_new_user(pool: &SqlitePool, user_id: &str, role_id: &str) -> Result<usize> {
    if !role_auto_grants(pool, role_id).await? {
        return Ok(0);
    }
    let mut n = 0;

    let plugins = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM plugins WHERE grant_by_default = 1 ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    for (id,) in plugins {
        plugin_access::grant(pool, &id, user_id).await?;
        n += 1;
    }

    let globals = sqlx::query_as::<_, (i64,)>(
        "SELECT id FROM mcp_global_servers WHERE grant_by_default = 1 ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    for (id,) in globals {
        mcp_global_access::grant(pool, id, user_id).await?;
        n += 1;
    }

    // Only `per_user` entries: `mcp_catalog_access` gates activation, and a
    // `global` entry is never activated by a user — a row for one would be dead.
    let catalog = sqlx::query_as::<_, (String,)>(
        "SELECT name FROM mcp_catalog
         WHERE  grant_by_default = 1 AND scope = 'per_user'
         ORDER  BY name",
    )
    .fetch_all(pool)
    .await?;
    for (name,) in catalog {
        mcp_catalog_access::grant(pool, &name, user_id).await?;
        n += 1;
    }

    Ok(n)
}

// ── Per-object opt-out ───────────────────────────────────────────────────────

/// Reads the object's own `grant_by_default`. A missing row answers `false`: the
/// object was deleted between insert and seed, and granting it would be a dangling
/// row in a junction whose FK does not always exist to catch it.
async fn object_grants_by_default(pool: &SqlitePool, target: Grantable<'_>) -> Result<bool> {
    let flag: Option<(i64,)> = match target {
        Grantable::Plugin(id) => {
            sqlx::query_as("SELECT grant_by_default FROM plugins WHERE id = ?")
                .bind(id)
                .fetch_optional(pool)
                .await?
        }
        Grantable::GlobalServer(id) => {
            sqlx::query_as("SELECT grant_by_default FROM mcp_global_servers WHERE id = ?")
                .bind(id)
                .fetch_optional(pool)
                .await?
        }
        // The scope guard lives here rather than in every call site: only a
        // `per_user` entry is ever activated by a user.
        Grantable::Catalog(name) => {
            sqlx::query_as(
                "SELECT grant_by_default FROM mcp_catalog WHERE name = ? AND scope = 'per_user'",
            )
            .bind(name)
            .fetch_optional(pool)
            .await?
        }
    };
    Ok(matches!(flag, Some((1,))))
}

/// Sets whether an object is auto-granted from now on. Changing it is **not**
/// retroactive in either direction — existing grants are the admin's, and the two
/// seeding moments are the only writers.
pub async fn set_grant_by_default(
    pool:    &SqlitePool,
    target:  Grantable<'_>,
    enabled: bool,
) -> Result<()> {
    let on = enabled as i64;
    match target {
        Grantable::Plugin(id) => {
            sqlx::query("UPDATE plugins SET grant_by_default = ? WHERE id = ?")
                .bind(on).bind(id).execute(pool).await?;
        }
        Grantable::GlobalServer(id) => {
            sqlx::query("UPDATE mcp_global_servers SET grant_by_default = ? WHERE id = ?")
                .bind(on).bind(id).execute(pool).await?;
        }
        Grantable::Catalog(name) => {
            sqlx::query("UPDATE mcp_catalog SET grant_by_default = ? WHERE name = ?")
                .bind(on).bind(name).execute(pool).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A registry database holding an admin, an adult member and a child, on the
    /// three roles the family profile seeds. FK enforcement is on, so roles exist
    /// before users and catalog entries before grants.
    async fn fixture(tag: &str) -> (SqlitePool, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("skald-accessdefaults-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::init_system_pool(&dir.join("system.db").to_string_lossy())
            .await
            .unwrap();

        // `member` leaves auto_grant unset — it must still behave as `true`.
        roles::insert(&pool, "member", "Member", "default", Some(r#"{"ui_mode":"full"}"#))
            .await.unwrap();
        roles::insert(&pool, "children", "Children", "default",
            Some(r#"{"ui_mode":"simple","auto_grant":false}"#)).await.unwrap();
        for (id, name, role) in [
            ("u_admin", "ada", "admin"),
            ("u_adult", "bob", "member"),
            ("u_kid",   "kim", "children"),
        ] {
            sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES (?, ?, ?, 0)")
                .bind(id).bind(name).bind(role).execute(&pool).await.unwrap();
        }
        (pool, dir)
    }

    async fn add_plugin(pool: &SqlitePool, id: &str) {
        crate::db::plugins::upsert(pool, id, true, "{}").await.unwrap();
    }

    async fn add_catalog(pool: &SqlitePool, name: &str, scope: &str) {
        sqlx::query("INSERT INTO mcp_catalog (name, scope, source) VALUES (?, ?, 'remote')")
            .bind(name).bind(scope).execute(pool).await.unwrap();
    }

    async fn add_global(pool: &SqlitePool, name: &str) -> i64 {
        sqlx::query("INSERT INTO mcp_global_servers (name) VALUES (?)")
            .bind(name).execute(pool).await.unwrap().last_insert_rowid()
    }

    #[tokio::test]
    async fn a_new_object_reaches_auto_grant_roles_only() {
        let (pool, dir) = fixture("object").await;

        add_plugin(&pool, "telegram").await;
        let n = seed_new_object(&pool, Grantable::Plugin("telegram")).await.unwrap();

        // The adult gets it; the child's role opted out and the admin needs no row.
        assert_eq!(n, 1);
        assert!(plugin_access::has_access(&pool, "telegram", "u_adult").await.unwrap());
        assert!(!plugin_access::has_access(&pool, "telegram", "u_kid").await.unwrap());
        assert!(!plugin_access::has_access(&pool, "telegram", "u_admin").await.unwrap());

        // The same for a global connector and a per-user catalog entry.
        let sid = add_global(&pool, "tavily").await;
        seed_new_object(&pool, Grantable::GlobalServer(sid)).await.unwrap();
        assert!(mcp_global_access::has_access(&pool, sid, "u_adult").await.unwrap());
        assert!(!mcp_global_access::has_access(&pool, sid, "u_kid").await.unwrap());

        add_catalog(&pool, "gmail", "per_user").await;
        seed_new_object(&pool, Grantable::Catalog("gmail")).await.unwrap();
        assert!(mcp_catalog_access::has_access(&pool, "gmail", "u_adult").await.unwrap());
        assert!(!mcp_catalog_access::has_access(&pool, "gmail", "u_kid").await.unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_new_user_starts_with_every_default_on_object() {
        let (pool, dir) = fixture("user").await;
        add_plugin(&pool, "telegram").await;
        let sid = add_global(&pool, "tavily").await;
        add_catalog(&pool, "gmail", "per_user").await;
        // A `global` catalog entry is never user-activated — no row for it.
        add_catalog(&pool, "websearch", "global").await;

        // An adult joining later lands on the same set as everyone else.
        sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES ('u_new', 'eve', 'member', 0)")
            .execute(&pool).await.unwrap();
        let n = seed_new_user(&pool, "u_new", "member").await.unwrap();
        assert_eq!(n, 3);
        assert!(plugin_access::has_access(&pool, "telegram", "u_new").await.unwrap());
        assert!(mcp_global_access::has_access(&pool, sid, "u_new").await.unwrap());
        assert!(mcp_catalog_access::has_access(&pool, "gmail", "u_new").await.unwrap());
        assert!(!mcp_catalog_access::has_access(&pool, "websearch", "u_new").await.unwrap());

        // A child joining gets nothing, and neither does a new admin.
        assert_eq!(seed_new_user(&pool, "u_kid", "children").await.unwrap(), 0);
        assert_eq!(seed_new_user(&pool, "u_admin", "admin").await.unwrap(), 0);
        assert!(!plugin_access::has_access(&pool, "telegram", "u_kid").await.unwrap());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_opted_out_object_seeds_nobody_in_either_direction() {
        let (pool, dir) = fixture("optout").await;

        add_plugin(&pool, "mobile-connector").await;
        set_grant_by_default(&pool, Grantable::Plugin("mobile-connector"), false).await.unwrap();

        assert_eq!(seed_new_object(&pool, Grantable::Plugin("mobile-connector")).await.unwrap(), 0);
        assert!(!plugin_access::has_access(&pool, "mobile-connector", "u_adult").await.unwrap());

        // ...and it is skipped when a new user is seeded, too.
        sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES ('u_new', 'eve', 'member', 0)")
            .execute(&pool).await.unwrap();
        assert_eq!(seed_new_user(&pool, "u_new", "member").await.unwrap(), 0);

        // An object that vanished between insert and seed is a no-op, not an error.
        assert_eq!(seed_new_object(&pool, Grantable::Plugin("ghost")).await.unwrap(), 0);
        assert_eq!(seed_new_object(&pool, Grantable::GlobalServer(4242)).await.unwrap(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn seeding_is_idempotent_and_never_revokes() {
        let (pool, dir) = fixture("idempotent").await;
        add_plugin(&pool, "telegram").await;

        seed_new_object(&pool, Grantable::Plugin("telegram")).await.unwrap();
        // The admin takes it away from the one person who had it...
        plugin_access::revoke(&pool, "telegram", "u_adult").await.unwrap();
        // ...and re-running the seed is what a re-enable must never do. The call
        // site guards that (it only fires on row creation); this pins the fact
        // that seeding itself is purely additive and idempotent on the PK.
        seed_new_object(&pool, Grantable::Plugin("telegram")).await.unwrap();
        assert!(plugin_access::has_access(&pool, "telegram", "u_adult").await.unwrap());
        assert_eq!(plugin_access::users_for_plugin(&pool, "telegram").await.unwrap(), vec!["u_adult"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
