use std::sync::Arc;

use sqlx::SqlitePool;

use core_api::system_bus::{SystemEvent, SystemEventBus};

pub struct GlobalConfigManager {
    pool:       Arc<SqlitePool>,
    system_bus: Arc<SystemEventBus>,
}

impl GlobalConfigManager {
    pub fn new(pool: Arc<SqlitePool>, system_bus: Arc<SystemEventBus>) -> Self {
        Self { pool, system_bus }
    }

    pub async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        let row = sqlx::query_as::<_, (String,)>("SELECT value FROM config WHERE key = ?")
            .bind(key)
            .fetch_optional(&*self.pool)
            .await?;
        Ok(row.map(|(v,)| v))
    }

    /// Sets a config key and emits [`SystemEvent::ConfigKeyUpdated`] on the
    /// system bus when the value actually changes. No-op (no write, no event)
    /// when the new value equals the current one.
    pub async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let old_value = self.get(key).await?;

        if old_value.as_deref() == Some(value) {
            return Ok(());
        }

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

        self.system_bus.send(SystemEvent::ConfigKeyUpdated {
            key:       key.to_string(),
            old_value,
            new_value: value.to_string(),
        });

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

#[async_trait::async_trait]
impl core_api::config_api::ConfigApi for GlobalConfigManager {
    async fn get(&self, key: &str) -> anyhow::Result<Option<String>> {
        GlobalConfigManager::get(self, key).await
    }
    async fn set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        GlobalConfigManager::set(self, key, value).await
    }
}
