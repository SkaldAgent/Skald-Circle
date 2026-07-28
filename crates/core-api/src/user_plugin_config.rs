use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

/// Per-user plugin configuration store (`plugin_user_configs` table in
/// `system.db`).
///
/// Values are deliberately admin-readable — the table lives in the registry
/// database — so per-user plugin configs must never collect secrets. A plugin
/// that needs per-user secrets should keep them elsewhere.
#[async_trait]
pub trait PluginUserConfigApi: Send + Sync {
    async fn get(&self, plugin_id: &str, user_id: &str) -> Result<Option<Value>>;
    async fn set(&self, plugin_id: &str, user_id: &str, config: Value) -> Result<()>;
    async fn delete(&self, plugin_id: &str, user_id: &str) -> Result<()>;
}
