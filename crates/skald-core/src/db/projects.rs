use anyhow::Result;
use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Project {
    pub id:          i64,
    pub name:        String,
    pub path:        String,
    pub description: String,
    pub run_context: Option<String>,
    pub created_at:  String,
    pub updated_at:  String,
}

const SELECT: &str =
    "SELECT id, name, path, description, run_context, created_at, updated_at
     FROM projects";

pub async fn list(pool: &SqlitePool) -> Result<Vec<Project>> {
    let rows = sqlx::query_as::<_, Project>(sqlx::AssertSqlSafe(format!(
        "{SELECT} ORDER BY updated_at DESC"
    )))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Project>> {
    let row = sqlx::query_as::<_, Project>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE id = ?")))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn create(
    pool:        &SqlitePool,
    name:        &str,
    path:        &str,
    description: &str,
    run_context: Option<&str>,
) -> Result<Project> {
    let id = sqlx::query(
        "INSERT INTO projects (name, path, description, run_context)
         VALUES (?, ?, ?, ?)",
    )
    .bind(name)
    .bind(path)
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

pub async fn update(
    pool:        &SqlitePool,
    id:          i64,
    name:        &str,
    path:        &str,
    description: &str,
    run_context: Option<&str>,
) -> Result<bool> {
    let n = sqlx::query(
        "UPDATE projects
         SET name = ?, path = ?, description = ?, run_context = ?,
             updated_at = datetime('now')
         WHERE id = ?",
    )
    .bind(name)
    .bind(path)
    .bind(description)
    .bind(run_context)
    .bind(id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// Touch updated_at — called after every ticket operation so ordering by recency works.
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
