use sqlx::SqlitePool;

pub struct ConfigEntry {
    pub key:        String,
    pub value:      String,
    pub updated_at: String,
}

/// Get a config value by key.
pub async fn get(pool: &SqlitePool, key: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT value FROM config WHERE key = ?",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(v,)| v))
}

/// Upsert a config key/value pair.
pub async fn set(pool: &SqlitePool, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO config (key, value, updated_at)
         VALUES (?, ?, datetime('now'))
         ON CONFLICT(key) DO UPDATE SET
             value      = excluded.value,
             updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete a config entry.
pub async fn delete(pool: &SqlitePool, key: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM config WHERE key = ?")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}
