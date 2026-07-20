//! Project membership (registry / `system.db`): who can reach a project and with what
//! capability. Mirrors [`super::shared_folders`]'s membership model — a junction table
//! so a member can be read-only, and so both the container mount topology and the
//! "shared with me / owner badge" list can query it in either direction.
//!
//! Two path segments distinguish it from a shared folder: a project lives at
//! `projects/{owner_username}/{slug}`, so the mount rows carry the owner's **userid**
//! (the host path segment, stable) and **username** (the agent-visible segment).
//! FK `user_id → users(id)` is registry→registry (same file) — allowed.

use anyhow::Result;
use serde::Serialize;
use sqlx::SqlitePool;

/// One project a user can reach, resolved for building their container mounts.
#[derive(Debug, Clone)]
pub struct ProjectMountRow {
    pub project_id:     i64,
    /// Owner's userid — the **host** path segment (`{WD}/projects/{owner_userid}/{slug}`).
    pub owner_user_id:  String,
    /// Owner's username — the **agent-visible / container** path segment.
    pub owner_username: String,
    pub slug:           String,
    pub can_write:      bool,
}

/// A project as it appears in a user's list: identity, owner, the caller's capability,
/// and whether the caller owns it. `owner_name` is `display_name || username`.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectAccess {
    pub id:            i64,
    pub name:          String,
    pub slug:          String,
    pub description:   String,
    pub owner_user_id: String,
    pub owner_name:    String,
    pub is_owner:      bool,
    pub can_write:     bool,
    pub updated_at:    String,
}

/// One member of a project — used by the share panel and the mount topology.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectMember {
    pub user_id:   String,
    pub can_write: bool,
}

// ── Reads ──────────────────────────────────────────────────────────────────────

/// Every project a user belongs to, resolved for their container mounts (owner's
/// userid + username + slug + capability). Drives `build_user_fs`.
pub async fn list_for_user_mounts(pool: &SqlitePool, user_id: &str) -> Result<Vec<ProjectMountRow>> {
    let rows = sqlx::query_as::<_, (i64, String, String, String, i64)>(
        "SELECT p.id, p.owner_user_id, u.username, p.slug, m.can_write
         FROM   project_members m
         JOIN   projects p ON p.id = m.project_id
         JOIN   users u ON u.id = p.owner_user_id
         WHERE  m.user_id = ?
         ORDER  BY u.username, p.slug",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(project_id, owner_user_id, owner_username, slug, can_write)| ProjectMountRow {
            project_id,
            owner_user_id,
            owner_username,
            slug,
            can_write: can_write != 0,
        })
        .collect())
}

/// The projects a user can see (owned + shared-with-them), for the UI list. Ordered by
/// recency. `is_owner` distinguishes owned from shared (the owner-badge signal).
pub async fn list_for_user(pool: &SqlitePool, user_id: &str) -> Result<Vec<ProjectAccess>> {
    let rows = sqlx::query_as::<_, (i64, String, String, String, String, String, i64, i64, String)>(
        "SELECT p.id, p.name, p.slug, p.description, p.owner_user_id,
                COALESCE(NULLIF(ou.display_name, ''), ou.username) AS owner_name,
                (p.owner_user_id = ?) AS is_owner,
                m.can_write, p.updated_at
         FROM   project_members m
         JOIN   projects p ON p.id = m.project_id
         JOIN   users ou ON ou.id = p.owner_user_id
         WHERE  m.user_id = ?
         ORDER  BY p.updated_at DESC",
    )
    .bind(user_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, slug, description, owner_user_id, owner_name, is_owner, can_write, updated_at)| {
            ProjectAccess {
                id,
                name,
                slug,
                description,
                owner_user_id,
                owner_name,
                is_owner: is_owner != 0,
                can_write: can_write != 0,
                updated_at,
            }
        })
        .collect())
}

/// The members of a project — the set of users whose containers mount it.
pub async fn members(pool: &SqlitePool, project_id: i64) -> Result<Vec<ProjectMember>> {
    let rows = sqlx::query_as::<_, (String, i64)>(
        "SELECT user_id, can_write FROM project_members WHERE project_id = ?",
    )
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(user_id, can_write)| ProjectMember { user_id, can_write: can_write != 0 })
        .collect())
}

/// The caller's capability on a project: `None` when not a member, `Some(can_write)`
/// otherwise. The authority check for reads (member) and writes/share (write-member).
pub async fn capability_of(pool: &SqlitePool, project_id: i64, user_id: &str) -> Result<Option<bool>> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT can_write FROM project_members WHERE project_id = ? AND user_id = ?",
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(w,)| w != 0))
}

// ── Writes ─────────────────────────────────────────────────────────────────────

/// Adds (or updates the capability of) a member. Idempotent on the PK.
pub async fn add_member(
    pool:       &SqlitePool,
    project_id: i64,
    user_id:    &str,
    can_write:  bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO project_members (project_id, user_id, can_write)
         VALUES (?, ?, ?)
         ON CONFLICT (project_id, user_id) DO UPDATE SET can_write = excluded.can_write",
    )
    .bind(project_id)
    .bind(user_id)
    .bind(can_write as i64)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn remove_member(pool: &SqlitePool, project_id: i64, user_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM project_members WHERE project_id = ? AND user_id = ?")
        .bind(project_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn registry_pool(tag: &str) -> (SqlitePool, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("skald-projectmembers-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::init_system_pool(&dir.join("system.db").to_string_lossy())
            .await
            .unwrap();
        (pool, dir)
    }

    /// Proves the registry-→registry FK `project_members.user_id → users(id)` inserts
    /// with `PRAGMA foreign_keys=ON`, and that owned/shared are distinguished.
    #[tokio::test]
    async fn membership_list_distinguishes_owner_and_shared() {
        let (pool, dir) = registry_pool("list").await;
        for (id, name, display) in
            [("u1", "alice", None), ("u2", "bob", Some("Bob"))]
        {
            sqlx::query("INSERT INTO users (id, username, display_name, role_id, encrypted) VALUES (?, ?, ?, 'admin', 0)")
                .bind(id)
                .bind(name)
                .bind(display)
                .execute(&pool)
                .await
                .unwrap();
        }

        // Alice owns "budget", is a write-member of her own project.
        let p = super::super::projects::create(&pool, "u1", "Budget", "budget", "the money", None)
            .await
            .unwrap();
        add_member(&pool, p.id, "u1", true).await.unwrap();
        // Shared read-only with Bob.
        add_member(&pool, p.id, "u2", false).await.unwrap();

        let alice = list_for_user(&pool, "u1").await.unwrap();
        assert_eq!(alice.len(), 1);
        assert!(alice[0].is_owner);
        assert!(alice[0].can_write);
        assert_eq!(alice[0].owner_name, "alice");

        let bob = list_for_user(&pool, "u2").await.unwrap();
        assert_eq!(bob.len(), 1);
        assert!(!bob[0].is_owner);
        assert!(!bob[0].can_write);
        assert_eq!(bob[0].owner_name, "alice"); // owner is alice (no display name)

        // Mount rows carry both userid and username of the owner.
        let mounts = list_for_user_mounts(&pool, "u2").await.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].owner_user_id, "u1");
        assert_eq!(mounts[0].owner_username, "alice");
        assert_eq!(mounts[0].slug, "budget");
        assert!(!mounts[0].can_write);

        assert_eq!(capability_of(&pool, p.id, "u2").await.unwrap(), Some(false));
        assert_eq!(capability_of(&pool, p.id, "nobody").await.unwrap(), None);

        drop(pool);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
