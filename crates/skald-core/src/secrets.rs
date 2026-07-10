use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::SqlitePool;
use tracing::debug;

pub use core_api::secrets::{SecretsApi, require};

// ── SecretsStore ──────────────────────────────────────────────────────────────

pub struct SecretsStore {
    pool: Arc<SqlitePool>,
}

impl SecretsStore {
    pub fn new(pool: Arc<SqlitePool>) -> Arc<Self> {
        Arc::new(Self { pool })
    }
}

#[async_trait]
impl SecretsApi for SecretsStore {
    async fn get(&self, key: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT value FROM secrets WHERE key = ?1",
        )
        .bind(key)
        .fetch_optional(&*self.pool)
        .await
        .ok()
        .flatten()
    }

    async fn set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO secrets (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                            updated_at = datetime('now')",
        )
        .bind(key)
        .bind(value)
        .execute(&*self.pool)
        .await?;
        debug!(key, "secret set");
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM secrets WHERE key = ?1")
            .bind(key)
            .execute(&*self.pool)
            .await?;
        debug!(key, "secret deleted");
        Ok(())
    }

    async fn list_keys(&self) -> Vec<String> {
        sqlx::query_scalar::<_, String>("SELECT key FROM secrets ORDER BY key ASC")
            .fetch_all(&*self.pool)
            .await
            .unwrap_or_default()
    }
}
