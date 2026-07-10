use anyhow::Result;
use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct PluginRow {
    pub id:      String,
    pub enabled: bool,
    pub config:  String,  // JSON blob
}

/// Returns (enabled, config_json) for a plugin, or None if not yet in DB.
pub async fn get(pool: &SqlitePool, id: &str) -> Result<Option<PluginRow>> {
    let row: Option<(String, i64, String)> = sqlx::query_as(
        "SELECT id, enabled, config FROM plugins WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id, e, config)| PluginRow { id, enabled: e != 0, config }))
}

/// Upserts both enabled flag and config JSON.
pub async fn upsert(pool: &SqlitePool, id: &str, enabled: bool, config: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO plugins (id, enabled, config)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET enabled = excluded.enabled,
                                       config  = excluded.config",
    )
    .bind(id)
    .bind(enabled as i64)
    .bind(config)
    .execute(pool)
    .await?;
    Ok(())
}

/// Returns all plugin rows. Used by the config watcher.
pub async fn list(pool: &SqlitePool) -> Result<Vec<PluginRow>> {
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT id, enabled, config FROM plugins ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, e, config)| PluginRow { id, enabled: e != 0, config })
        .collect())
}
