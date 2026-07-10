use anyhow::{Context, Result};
use sqlx::SqlitePool;

use super::TtsModelRecord;

#[derive(sqlx::FromRow)]
struct TtsModelRow {
    id:           i64,
    provider_id:  i64,
    model_id:     String,
    voice_id:     Option<String>,
    name:            String,
    description:     Option<String>,
    instructions:    Option<String>,
    response_format: Option<String>,
    priority:        i64,
}

pub async fn load_all(pool: &SqlitePool) -> Result<Vec<TtsModelRecord>> {
    let rows = sqlx::query_as::<_, TtsModelRow>(
        "SELECT id, provider_id, model_id, voice_id, name, description, instructions, response_format, priority
         FROM tts_models
         WHERE removed_at IS NULL
         ORDER BY priority ASC, name ASC",
    )
    .fetch_all(pool)
    .await
    .context("tts_models: load_all")?;

    Ok(rows.into_iter().map(row_to_record).collect())
}

pub async fn insert(pool: &SqlitePool, r: &TtsModelRecord) -> Result<i64> {
    // Revive a soft-deleted row that collides on (provider_id, model_id) or name
    // before attempting a plain INSERT, which would fail on the UNIQUE constraints.
    let restored = sqlx::query_scalar::<_, i64>(
        "UPDATE tts_models
         SET provider_id=?1, model_id=?2, voice_id=?3, name=?4,
             description=?5, instructions=?6, priority=?7, response_format=?8, removed_at=NULL
         WHERE id = (
             SELECT id FROM tts_models
             WHERE removed_at IS NOT NULL
               AND (provider_id=?1 AND model_id=?2 OR name=?4)
             LIMIT 1
         )
         RETURNING id",
    )
    .bind(r.provider_id)
    .bind(&r.model_id)
    .bind(&r.voice_id)
    .bind(&r.name)
    .bind(&r.description)
    .bind(&r.instructions)
    .bind(r.priority as i64)
    .bind(&r.response_format)
    .fetch_optional(pool)
    .await
    .context("tts_models: restore soft-deleted")?;

    if let Some(id) = restored {
        return Ok(id);
    }

    sqlx::query_scalar::<_, i64>(
        "INSERT INTO tts_models (provider_id, model_id, voice_id, name, description, instructions, priority, response_format)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         RETURNING id",
    )
    .bind(r.provider_id)
    .bind(&r.model_id)
    .bind(&r.voice_id)
    .bind(&r.name)
    .bind(&r.description)
    .bind(&r.instructions)
    .bind(r.priority as i64)
    .bind(&r.response_format)
    .fetch_one(pool)
    .await
    .context("tts_models: insert")
}

pub async fn update(pool: &SqlitePool, id: i64, r: &TtsModelRecord) -> Result<()> {
    sqlx::query(
        "UPDATE tts_models
         SET provider_id=?1, model_id=?2, voice_id=?3, name=?4, description=?5, instructions=?6, priority=?7, response_format=?8
         WHERE id=?9",
    )
    .bind(r.provider_id)
    .bind(&r.model_id)
    .bind(&r.voice_id)
    .bind(&r.name)
    .bind(&r.description)
    .bind(&r.instructions)
    .bind(r.priority as i64)
    .bind(&r.response_format)
    .bind(id)
    .execute(pool)
    .await
    .context("tts_models: update")?;
    Ok(())
}

pub async fn soft_delete(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE tts_models SET removed_at = datetime('now') WHERE id = ?1",
    )
    .bind(id)
    .execute(pool)
    .await
    .context("tts_models: soft-delete")?;
    Ok(())
}

fn row_to_record(r: TtsModelRow) -> TtsModelRecord {
    TtsModelRecord {
        id:           r.id,
        provider_id:  r.provider_id,
        model_id:     r.model_id,
        voice_id:     r.voice_id,
        name:            r.name,
        description:     r.description,
        instructions:    r.instructions,
        response_format: r.response_format,
        priority:        r.priority as i32,
    }
}
