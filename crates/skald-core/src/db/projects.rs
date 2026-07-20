//! Projects: shareable endeavours over an on-disk folder (registry / `system.db`).
//!
//! A project is an *endeavour* (owner + membership + metadata) that HAS a *place*:
//! a folder `{WD}/projects/{owner_userid}/{slug}` bind-mounted into each member's
//! container (the membership lives in [`super::project_members`]). This module owns
//! the `projects` row itself. Registry table — metadata is **not** encrypted (§2/§6);
//! only user↔agent conversations stay in the per-user encrypted DB.

use anyhow::Result;
use sqlx::SqlitePool;

/// A project row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Project {
    pub id:            i64,
    pub owner_user_id: String,
    /// Display name (free text).
    pub name:          String,
    /// Path component — the on-disk folder + agent-visible segment. Immutable.
    pub slug:          String,
    pub description:   String,
    pub run_context:   Option<String>,
    pub created_at:    String,
    pub updated_at:    String,
}

const SELECT: &str =
    "SELECT id, owner_user_id, name, slug, description, run_context, created_at, updated_at
     FROM projects";

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Project>> {
    let row = sqlx::query_as::<_, Project>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE id = ?")))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

/// Creates a project and returns the new row. The caller must ensure `slug` is valid
/// ([`is_valid_slug`]) and unique for `owner_user_id` ([`unique_slug`]); the owner is
/// added to `project_members` separately (so mounts are uniform).
pub async fn create(
    pool:          &SqlitePool,
    owner_user_id: &str,
    name:          &str,
    slug:          &str,
    description:   &str,
    run_context:   Option<&str>,
) -> Result<Project> {
    let id = sqlx::query(
        "INSERT INTO projects (owner_user_id, name, slug, description, run_context)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(owner_user_id)
    .bind(name)
    .bind(slug)
    .bind(description)
    .bind(run_context)
    .execute(pool)
    .await?
    .last_insert_rowid();

    let row = sqlx::query_as::<_, Project>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE id = ?")))
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

/// Updates the mutable fields. `slug` and `owner_user_id` are immutable — changing the
/// slug would move the on-disk folder and break every member's path.
pub async fn update(
    pool:        &SqlitePool,
    id:          i64,
    name:        &str,
    description: &str,
    run_context: Option<&str>,
) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE projects
         SET name = ?, description = ?, run_context = ?, updated_at = datetime('now')
         WHERE id = ?",
    )
    .bind(name)
    .bind(description)
    .bind(run_context)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// Touch `updated_at` so recency ordering works.
pub async fn touch(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("UPDATE projects SET updated_at = datetime('now') WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, id: i64) -> Result<bool> {
    let n = sqlx::query("DELETE FROM projects WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n > 0)
}

// ── Slug helpers ───────────────────────────────────────────────────────────────

/// A slug must be a single safe path component: it becomes a real directory
/// `{WD}/projects/{owner}/{slug}` and a `docker` mount target, so it may not be
/// empty, be a `.`/`..` traversal, or contain a separator. Same rule as
/// [`super::shared_folders::is_valid_folder_name`].
pub fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug != "."
        && slug != ".."
        && !slug.contains('/')
        && !slug.contains('\\')
        && !slug.contains('\0')
}

/// Best-effort slugify of a display name: lowercase ASCII alphanumerics, every other
/// run collapsed to a single `-`, trimmed. Falls back to `project` when nothing is left.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() { "project".to_string() } else { trimmed.to_string() }
}

/// Returns a slug unique within `owner_user_id`, appending `-2`, `-3`, … on collision.
pub async fn unique_slug(pool: &SqlitePool, owner_user_id: &str, base: &str) -> Result<String> {
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT slug FROM projects WHERE owner_user_id = ?",
    )
    .bind(owner_user_id)
    .fetch_all(pool)
    .await?;
    if !existing.iter().any(|s| s == base) {
        return Ok(base.to_string());
    }
    let mut n = 2;
    loop {
        let cand = format!("{base}-{n}");
        if !existing.iter().any(|s| s == &cand) {
            return Ok(cand);
        }
        n += 1;
    }
}
