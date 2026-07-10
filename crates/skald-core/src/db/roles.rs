use anyhow::{Result, bail};
use serde::Serialize;
use sqlx::SqlitePool;

/// The built-in admin role — immutable from the API.
pub const ADMIN_ROLE_ID: &str = "admin";

#[derive(Debug, Clone, Serialize)]
pub struct Role {
    pub id:               String,
    pub label:            String,
    pub permission_group: String,
    pub attrs:            Option<String>,
    pub created_at:       String,
}

type RawRow = (String, String, String, Option<String>, String);

fn from_raw((id, label, permission_group, attrs, created_at): RawRow) -> Role {
    Role { id, label, permission_group, attrs, created_at }
}

// ── Reads ────────────────────────────────────────────────────────────────────

pub async fn list(pool: &SqlitePool) -> Result<Vec<Role>> {
    let rows = sqlx::query_as::<_, RawRow>(
        "SELECT id, label, permission_group, attrs, created_at
         FROM   roles
         ORDER  BY label",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(from_raw).collect())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<Role>> {
    let row = sqlx::query_as::<_, RawRow>(
        "SELECT id, label, permission_group, attrs, created_at
         FROM   roles WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(from_raw))
}

// ── Writes ───────────────────────────────────────────────────────────────────

pub async fn insert(
    pool:             &SqlitePool,
    id:               &str,
    label:            &str,
    permission_group: &str,
    attrs:            Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO roles (id, label, permission_group, attrs)
         VALUES (?, ?, ?, ?)",
    )
    .bind(id)
    .bind(label)
    .bind(permission_group)
    .bind(attrs)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update(
    pool:             &SqlitePool,
    id:               &str,
    label:            &str,
    permission_group: &str,
    attrs:            Option<&str>,
) -> Result<bool> {
    let rows = sqlx::query(
        "UPDATE roles SET label = ?, permission_group = ?, attrs = ? WHERE id = ?",
    )
    .bind(label)
    .bind(permission_group)
    .bind(attrs)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows > 0)
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<()> {
    if id == ADMIN_ROLE_ID {
        bail!("cannot delete the built-in admin role");
    }
    let rows = sqlx::query("DELETE FROM roles WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    if rows == 0 {
        bail!("no such role: {id}");
    }
    Ok(())
}

/// How many users are assigned to a role — prevents deletion when non-zero.
pub async fn user_count(pool: &SqlitePool, role_id: &str) -> Result<i64> {
    let (n,) = sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM users WHERE role_id = ?")
        .bind(role_id)
        .fetch_one(pool)
        .await?;
    Ok(n)
}

// ── Seed ─────────────────────────────────────────────────────────────────────

/// Inserts the built-in `admin` role. Idempotent.
pub async fn seed_admin(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO roles (id, label, permission_group)
         VALUES ('admin', 'Administrator', 'default')",
    )
    .execute(pool)
    .await?;
    Ok(())
}
