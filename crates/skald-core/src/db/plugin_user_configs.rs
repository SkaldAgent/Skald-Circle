//! Per-user plugin configuration blobs (`plugin_user_configs` table).
//!
//! Registry table in `system.db` — **admin-readable, never secrets**. A plugin
//! with a non-empty `user_config_schema()` lets each granted user submit their
//! own settings from the UI (e.g. Telegram's pairing code); the plugin's
//! `update_user_config` hook validates and stores here. `plugin_id` is a bare
//! TEXT for the same reason as `plugin_access`.

use anyhow::Result;
use serde_json::Value;
use sqlx::SqlitePool;

pub async fn get(pool: &SqlitePool, plugin_id: &str, user_id: &str) -> Result<Option<Value>> {
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT config FROM plugin_user_configs WHERE plugin_id = ? AND user_id = ?",
    )
    .bind(plugin_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    match row {
        None => Ok(None),
        Some((json,)) => Ok(Some(serde_json::from_str(&json)?)),
    }
}

pub async fn set(pool: &SqlitePool, plugin_id: &str, user_id: &str, config: &Value) -> Result<()> {
    sqlx::query(
        "INSERT INTO plugin_user_configs (plugin_id, user_id, config, updated_at)
         VALUES (?1, ?2, ?3, datetime('now'))
         ON CONFLICT(plugin_id, user_id)
         DO UPDATE SET config = excluded.config, updated_at = excluded.updated_at",
    )
    .bind(plugin_id)
    .bind(user_id)
    .bind(serde_json::to_string(config)?)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, plugin_id: &str, user_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM plugin_user_configs WHERE plugin_id = ? AND user_id = ?")
        .bind(plugin_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}
