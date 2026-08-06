//! One owner's own key/value preferences, in their own database.
//!
//! The per-user twin of [`super::config`]: same shape, different file and a
//! different name on purpose (see the table comment in
//! [`super::create_owner_tables`]). Anything scoped to a person — the surface
//! their notifications go to, say — belongs here; instance-wide settings the
//! admin owns stay in the registry `config` table.

use sqlx::SqlitePool;

/// Get a value by key from this owner's database.
pub async fn get(pool: &SqlitePool, key: &str) -> anyhow::Result<Option<String>> {
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT value FROM user_config WHERE key = ?",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(v,)| v))
}

/// Upsert a key/value pair in this owner's database.
pub async fn set(pool: &SqlitePool, key: &str, value: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO user_config (key, value, updated_at)
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

/// Delete an entry.
pub async fn delete(pool: &SqlitePool, key: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM user_config WHERE key = ?")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}
