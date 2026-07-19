//! Shared on-disk folders and their membership (blueprint §6 / §0.1).
//!
//! A shared folder is a named directory `{WD}/shared/{folder_name}` bind-mounted
//! into the container of each of its members. Membership is a junction table so a
//! member can be read-only (`can_write = 0`) and so both the mount topology and
//! the fs-tool router can query it in either direction. Registry tables — they
//! live in `system.db` and carry no user key material.

use anyhow::Result;
use serde::Serialize;
use sqlx::SqlitePool;

/// A shared folder row.
#[derive(Debug, Clone, Serialize)]
pub struct SharedFolder {
    pub id:          i64,
    pub folder_name: String,
    /// What the folder holds — injected into the agent's system context so it
    /// knows what to store here and when to read it. Admin-authored (§6).
    pub description: String,
    pub created_at:  String,
}

/// One folder a given user can reach, with the capability they hold on it.
#[derive(Debug, Clone, Serialize)]
pub struct SharedMembership {
    pub folder_id:   i64,
    pub folder_name: String,
    pub can_write:   bool,
}

/// One member of a folder — used to build the folder's mount topology.
#[derive(Debug, Clone, Serialize)]
pub struct FolderMember {
    pub user_id:   String,
    pub can_write: bool,
}

/// A shared folder as the agent sees it: path component, the caller's
/// capability on it, who else it is shared with, and the admin-authored
/// description. Rendered into the system prompt by the `<!-- SHARED_FOLDERS -->`
/// directive.
#[derive(Debug, Clone, Serialize)]
pub struct SharedFolderAccess {
    pub folder_name: String,
    pub can_write:   bool,
    /// Names of the folder's *other* members (the caller excluded), joined by
    /// `", "` — empty when the caller is the sole member.
    pub shared_with: String,
    pub description: String,
}

// ── Reads ────────────────────────────────────────────────────────────────────

/// Every shared folder a user belongs to, with their per-folder capability.
/// Drives both the user's container mounts and the fs-tool `shared/{X}` routing.
pub async fn list_for_user(pool: &SqlitePool, user_id: &str) -> Result<Vec<SharedMembership>> {    let rows = sqlx::query_as::<_, (i64, String, i64)>(
        "SELECT f.id, f.folder_name, m.can_write
         FROM   shared_folder_members m
         JOIN   shared_folders f ON f.id = m.folder_id
         WHERE  m.user_id = ?
         ORDER  BY f.folder_name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(folder_id, folder_name, can_write)| SharedMembership {
            folder_id,
            folder_name,
            can_write: can_write != 0,
        })
        .collect())
}

/// The folders a user belongs to, with capability, the other members' names,
/// and description — the row set rendered by the `<!-- SHARED_FOLDERS -->`
/// prompt directive. Same join as [`list_for_user`], plus the agent-facing
/// columns. `shared_with` names the *other* members (display name when set,
/// username otherwise) so the prompt can state exactly who sees what.
pub async fn agent_view(pool: &SqlitePool, user_id: &str) -> Result<Vec<SharedFolderAccess>> {
    let rows = sqlx::query_as::<_, (String, i64, String, String)>(
        "SELECT f.folder_name, m.can_write,
                COALESCE((SELECT GROUP_CONCAT(name, ', ') FROM (
                             SELECT COALESCE(NULLIF(u2.display_name, ''), u2.username) AS name
                             FROM   shared_folder_members m2
                             JOIN   users u2 ON u2.id = m2.user_id
                             WHERE  m2.folder_id = f.id AND m2.user_id != ?
                             ORDER  BY name
                         )), '') AS shared_with,
                f.description
         FROM   shared_folder_members m
         JOIN   shared_folders f ON f.id = m.folder_id
         WHERE  m.user_id = ?
         ORDER  BY f.folder_name",
    )
    .bind(user_id)
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(folder_name, can_write, shared_with, description)| SharedFolderAccess {
            folder_name,
            can_write: can_write != 0,
            shared_with,
            description,
        })
        .collect())
}

pub async fn list_all(pool: &SqlitePool) -> Result<Vec<SharedFolder>> {
    let rows = sqlx::query_as::<_, (i64, String, String, String)>(
        "SELECT id, folder_name, description, created_at FROM shared_folders ORDER BY folder_name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, folder_name, description, created_at)| SharedFolder {
            id,
            folder_name,
            description,
            created_at,
        })
        .collect())
}

pub async fn get(pool: &SqlitePool, folder_id: i64) -> Result<Option<SharedFolder>> {
    let row = sqlx::query_as::<_, (i64, String, String, String)>(
        "SELECT id, folder_name, description, created_at FROM shared_folders WHERE id = ?",
    )
    .bind(folder_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, folder_name, description, created_at)| SharedFolder {
        id,
        folder_name,
        description,
        created_at,
    }))
}

pub async fn get_by_name(pool: &SqlitePool, folder_name: &str) -> Result<Option<SharedFolder>> {
    let row = sqlx::query_as::<_, (i64, String, String, String)>(
        "SELECT id, folder_name, description, created_at FROM shared_folders WHERE folder_name = ?",
    )
    .bind(folder_name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, folder_name, description, created_at)| SharedFolder {
        id,
        folder_name,
        description,
        created_at,
    }))
}

/// The members of a folder — the set of users whose containers mount it.
pub async fn members(pool: &SqlitePool, folder_id: i64) -> Result<Vec<FolderMember>> {
    let rows = sqlx::query_as::<_, (String, i64)>(
        "SELECT user_id, can_write FROM shared_folder_members WHERE folder_id = ?",
    )
    .bind(folder_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(user_id, can_write)| FolderMember { user_id, can_write: can_write != 0 })
        .collect())
}

// ── Writes ───────────────────────────────────────────────────────────────────

/// Creates a folder, returning its id. `folder_name` must already be validated as
/// a safe path component (see [`is_valid_folder_name`]).
pub async fn create(pool: &SqlitePool, folder_name: &str, description: &str) -> Result<i64> {
    let id = sqlx::query("INSERT INTO shared_folders (folder_name, description) VALUES (?, ?)")
        .bind(folder_name)
        .bind(description)
        .execute(pool)
        .await?
        .last_insert_rowid();
    Ok(id)
}

/// Updates a folder's description — the agent-facing text. No-op if `folder_id`
/// no longer exists.
pub async fn set_description(pool: &SqlitePool, folder_id: i64, description: &str) -> Result<()> {
    sqlx::query("UPDATE shared_folders SET description = ? WHERE id = ?")
        .bind(description)
        .bind(folder_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Adds (or updates the capability of) a member. Idempotent on the PK.
pub async fn add_member(
    pool:      &SqlitePool,
    folder_id: i64,
    user_id:   &str,
    can_write: bool,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO shared_folder_members (folder_id, user_id, can_write)
         VALUES (?, ?, ?)
         ON CONFLICT (folder_id, user_id) DO UPDATE SET can_write = excluded.can_write",
    )
    .bind(folder_id)
    .bind(user_id)
    .bind(can_write as i64)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn remove_member(pool: &SqlitePool, folder_id: i64, user_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM shared_folder_members WHERE folder_id = ? AND user_id = ?")
        .bind(folder_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, folder_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM shared_folders WHERE id = ?")
        .bind(folder_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ── Validation ───────────────────────────────────────────────────────────────

/// A folder name must be a single safe path component: it becomes a real
/// directory `{WD}/shared/{name}` and a `docker` mount target, so it may not be
/// empty, contain a path separator, or be a `.`/`..` traversal.
pub fn is_valid_folder_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A registry-schema database in a throwaway temp dir (mirrors the
    /// `owner_pool` helper in `memory_docs::tests`).
    async fn registry_pool(tag: &str) -> (SqlitePool, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("skald-sharedfolders-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::init_system_pool(&dir.join("system.db").to_string_lossy())
            .await
            .unwrap();
        (pool, dir)
    }

    #[tokio::test]
    async fn agent_view_returns_capability_members_and_description() {
        let (pool, dir) = registry_pool("agent-view").await;

        // `shared_folder_members.user_id` is a real FK and `tuned` turns FK
        // enforcement on, so members must exist (`admin` role is seeded).
        for (id, name, display) in
            [("u1", "alice", None), ("u2", "bob", Some("Bob")), ("u3", "carol", None)]
        {
            sqlx::query("INSERT INTO users (id, username, display_name, role_id, encrypted) VALUES (?, ?, ?, 'admin', 0)")
                .bind(id)
                .bind(name)
                .bind(display)
                .execute(&pool)
                .await
                .unwrap();
        }

        let recipes = create(&pool, "recipes", "Recipes and meal plans").await.unwrap();
        let photos = create(&pool, "photos", "").await.unwrap();
        add_member(&pool, recipes, "u1", true).await.unwrap();
        add_member(&pool, recipes, "u2", true).await.unwrap();
        add_member(&pool, recipes, "u3", false).await.unwrap();
        add_member(&pool, photos, "u1", false).await.unwrap();

        let rows = agent_view(&pool, "u1").await.unwrap();
        assert_eq!(rows.len(), 2);
        // Ordered by folder_name: photos first. Other members named by display
        // name when set, username otherwise, in name order; caller excluded.
        assert_eq!(rows[0].folder_name, "photos");
        assert!(!rows[0].can_write);
        assert_eq!(rows[0].shared_with, "");
        assert_eq!(rows[0].description, "");
        assert_eq!(rows[1].folder_name, "recipes");
        assert!(rows[1].can_write);
        assert_eq!(rows[1].shared_with, "Bob, carol");
        assert_eq!(rows[1].description, "Recipes and meal plans");

        // Bob's view of the same folder names the other side.
        let bob = agent_view(&pool, "u2").await.unwrap();
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0].shared_with, "alice, carol");

        // A non-member sees nothing.
        assert!(agent_view(&pool, "nobody").await.unwrap().is_empty());

        drop(pool);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
