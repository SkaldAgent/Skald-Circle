use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

/// Full CRUD interface to the secrets store.
///
/// Implemented by `SecretsStore` in the main crate. Both plugins (via
/// `PluginContext`) and agent tools (via `AppState`) receive an
/// `Arc<dyn SecretsApi>` so neither needs to depend on the main crate.
///
/// Security notes:
/// - Values should never appear in log output.
/// - Keys are safe to log/list.
/// - Storage is SQLite — same protection level as the rest of the DB.
#[async_trait]
pub trait SecretsApi: Send + Sync {
    /// Returns the value for `key`, or `None` if not set.
    async fn get(&self, key: &str) -> Option<String>;

    /// Inserts or replaces the secret for `key`.
    async fn set(&self, key: &str, value: &str) -> Result<()>;

    /// Removes the secret for `key`. No-op if not present.
    async fn delete(&self, key: &str) -> Result<()>;

    /// Returns all stored keys (never values).
    async fn list_keys(&self) -> Vec<String>;
}

/// Resolves a required secret, returning a descriptive error if absent.
pub async fn require(secrets: &Arc<dyn SecretsApi>, key: &str) -> Result<String> {
    secrets.get(key).await.ok_or_else(|| {
        anyhow::anyhow!(
            "secret '{}' is not set — tell the agent: \"set the secret {} to <value>\"",
            key, key,
        )
    })
}
