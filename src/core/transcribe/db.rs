use anyhow::{Context, Result};
use sqlx::SqlitePool;

use super::TranscribeModelRecord;

#[derive(sqlx::FromRow)]
struct TranscribeModelRow {
    id:          i64,
    provider_id: i64,
    model_id:    String,
    name:        String,
    language:    Option<String>,
    priority:    i64,
}

pub async fn load_all(pool: &SqlitePool) -> Result<Vec<TranscribeModelRecord>> {
    let rows = sqlx::query_as::<_, TranscribeModelRow>(
        "SELECT id, provider_id, model_id, name, language, priority
         FROM transcribe_models
         WHERE removed_at IS NULL
         ORDER BY priority ASC, name ASC",
    )
    .fetch_all(pool)
    .await
    .context("transcribe_models: load_all")?;

    Ok(rows.into_iter().map(row_to_record).collect())
}

pub async fn insert(pool: &SqlitePool, r: &TranscribeModelRecord) -> Result<i64> {
    let restored = sqlx::query_scalar::<_, i64>(
        "UPDATE transcribe_models
         SET provider_id=?1, model_id=?2, name=?3, language=?4, priority=?5, removed_at=NULL
         WHERE id = (
             SELECT id FROM transcribe_models
             WHERE removed_at IS NOT NULL
               AND (provider_id=?1 AND model_id=?2 OR name=?3)
             LIMIT 1
         )
         RETURNING id",
    )
    .bind(r.provider_id)
    .bind(&r.model_id)
    .bind(&r.name)
    .bind(&r.language)
    .bind(r.priority as i64)
    .fetch_optional(pool)
    .await
    .context("transcribe_models: restore soft-deleted")?;

    if let Some(id) = restored {
        return Ok(id);
    }

    sqlx::query_scalar::<_, i64>(
        "INSERT INTO transcribe_models (provider_id, model_id, name, language, priority)
         VALUES (?1, ?2, ?3, ?4, ?5)
         RETURNING id",
    )
    .bind(r.provider_id)
    .bind(&r.model_id)
    .bind(&r.name)
    .bind(&r.language)
    .bind(r.priority as i64)
    .fetch_one(pool)
    .await
    .context("transcribe_models: insert")
}

pub async fn update(pool: &SqlitePool, id: i64, r: &TranscribeModelRecord) -> Result<()> {
    sqlx::query(
        "UPDATE transcribe_models
         SET provider_id=?1, model_id=?2, name=?3, language=?4, priority=?5
         WHERE id=?6",
    )
    .bind(r.provider_id)
    .bind(&r.model_id)
    .bind(&r.name)
    .bind(&r.language)
    .bind(r.priority as i64)
    .bind(id)
    .execute(pool)
    .await
    .context("transcribe_models: update")?;
    Ok(())
}

pub async fn soft_delete(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE transcribe_models SET removed_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .execute(pool)
    .await
    .context("transcribe_models: soft-delete")?;
    Ok(())
}

fn row_to_record(r: TranscribeModelRow) -> TranscribeModelRecord {
    TranscribeModelRecord {
        id:          r.id,
        provider_id: r.provider_id,
        model_id:    r.model_id,
        name:        r.name,
        language:    r.language,
        priority:    r.priority as i32,
    }
}
