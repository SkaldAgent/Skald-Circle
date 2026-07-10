use std::sync::Arc;

use sqlx::SqlitePool;

pub struct GlobalConfigManager {
    pool: Arc<SqlitePool>,
}

impl GlobalConfigManager {
    pub fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }

    pub async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let row = sqlx::query_as::<_, (String,)>("SELECT value FROM config WHERE key = ?")
            .bind(key)
            .fetch_optional(&*self.pool)
            .await?;
        Ok(row.map(|(v,)| v))
    }

    pub async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO config (key, value, updated_at) VALUES (?, ?, datetime('now'))
             ON CONFLICT(key) DO UPDATE SET
                 value      = excluded.value,
                 updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .execute(&*self.pool)
        .await?;
        Ok(())
    }

    pub async fn remove(&self, key: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM config WHERE key = ?")
            .bind(key)
            .execute(&*self.pool)
            .await?;
        Ok(())
    }
}
