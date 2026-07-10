use anyhow::{Context, Result};
use sqlx::SqlitePool;

use super::ImageGenerateModelRecord;

#[derive(sqlx::FromRow)]
struct ImageGenerateModelRow {
    id:          i64,
    provider_id: i64,
    model_id:    String,
    name:        String,
    priority:    i64,
}

pub async fn load_all(pool: &SqlitePool) -> Result<Vec<ImageGenerateModelRecord>> {
    let rows = sqlx::query_as::<_, ImageGenerateModelRow>(
        "SELECT id, provider_id, model_id, name, priority
         FROM image_generate_models
         WHERE removed_at IS NULL
         ORDER BY priority ASC, name ASC",
    )
    .fetch_all(pool)
    .await
    .context("image_generate_models: load_all")?;

    Ok(rows.into_iter().map(row_to_record).collect())
}

pub async fn insert(pool: &SqlitePool, r: &ImageGenerateModelRecord) -> Result<i64> {
    let restored = sqlx::query_scalar::<_, i64>(
        "UPDATE image_generate_models
         SET provider_id=?1, model_id=?2, name=?3, priority=?4, removed_at=NULL
         WHERE id = (
             SELECT id FROM image_generate_models
             WHERE removed_at IS NOT NULL
               AND (provider_id=?1 AND model_id=?2 OR name=?3)
             LIMIT 1
         )
         RETURNING id",
    )
    .bind(r.provider_id)
    .bind(&r.model_id)
    .bind(&r.name)
    .bind(r.priority as i64)
    .fetch_optional(pool)
    .await
    .context("image_generate_models: restore soft-deleted")?;

    if let Some(id) = restored {
        return Ok(id);
    }

    sqlx::query_scalar::<_, i64>(
        "INSERT INTO image_generate_models (provider_id, model_id, name, priority)
         VALUES (?1, ?2, ?3, ?4)
         RETURNING id",
    )
    .bind(r.provider_id)
    .bind(&r.model_id)
    .bind(&r.name)
    .bind(r.priority as i64)
    .fetch_one(pool)
    .await
    .context("image_generate_models: insert")
}

pub async fn update(pool: &SqlitePool, id: i64, r: &ImageGenerateModelRecord) -> Result<()> {
    sqlx::query(
        "UPDATE image_generate_models
         SET provider_id=?1, model_id=?2, name=?3, priority=?4
         WHERE id=?5",
    )
    .bind(r.provider_id)
    .bind(&r.model_id)
    .bind(&r.name)
    .bind(r.priority as i64)
    .bind(id)
    .execute(pool)
    .await
    .context("image_generate_models: update")?;
    Ok(())
}

pub async fn soft_delete(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE image_generate_models SET removed_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .execute(pool)
    .await
    .context("image_generate_models: soft-delete")?;
    Ok(())
}

fn row_to_record(r: ImageGenerateModelRow) -> ImageGenerateModelRecord {
    ImageGenerateModelRecord {
        id:          r.id,
        provider_id: r.provider_id,
        model_id:    r.model_id,
        name:        r.name,
        priority:    r.priority as i32,
    }
}
