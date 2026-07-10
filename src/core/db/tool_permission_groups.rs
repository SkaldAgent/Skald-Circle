use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPermissionGroup {
    pub id:          String,
    pub name:        String,
    pub description: Option<String>,
    pub created_at:  String,
}

type RawRow = (String, String, Option<String>, String);

fn from_raw((id, name, description, created_at): RawRow) -> ToolPermissionGroup {
    ToolPermissionGroup { id, name, description, created_at }
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<ToolPermissionGroup>> {
    let rows = sqlx::query_as::<_, RawRow>(
        "SELECT id, name, description, created_at
         FROM   tool_permission_groups
         ORDER  BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(from_raw).collect())
}

pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<ToolPermissionGroup>> {
    let row = sqlx::query_as::<_, RawRow>(
        "SELECT id, name, description, created_at
         FROM   tool_permission_groups WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(from_raw))
}

pub async fn insert(pool: &SqlitePool, id: &str, name: &str, description: Option<&str>) -> Result<()> {
    sqlx::query(
        "INSERT INTO tool_permission_groups (id, name, description) VALUES (?, ?, ?)",
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn insert_or_ignore(pool: &SqlitePool, id: &str, name: &str, description: Option<&str>) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO tool_permission_groups (id, name, description) VALUES (?, ?, ?)",
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update(pool: &SqlitePool, id: &str, name: &str, description: Option<&str>) -> Result<bool> {
    let rows = sqlx::query(
        "UPDATE tool_permission_groups SET name = ?, description = ? WHERE id = ?",
    )
    .bind(name)
    .bind(description)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows > 0)
}

pub async fn delete(pool: &SqlitePool, id: &str) -> Result<bool> {
    let rows = sqlx::query("DELETE FROM tool_permission_groups WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(rows > 0)
}
