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

// ── Reads ────────────────────────────────────────────────────────────────────

/// Every shared folder a user belongs to, with their per-folder capability.
/// Drives both the user's container mounts and the fs-tool `shared/{X}` routing.
pub async fn list_for_user(pool: &SqlitePool, user_id: &str) -> Result<Vec<SharedMembership>> {
    let rows = sqlx::query_as::<_, (i64, String, i64)>(
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
