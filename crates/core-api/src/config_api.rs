use anyhow::Result;
use async_trait::async_trait;

/// Read/write access to the instance-wide key/value config store
/// (`config` table in `system.db`).
///
/// [`ConfigApi::set`] emits `ConfigKeyUpdated` on the system bus when the
/// value changes, so subscribers (e.g. the Telegram plugin reloading its
/// bindings) are notified without polling.
#[async_trait]
pub trait ConfigApi: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>>;
    async fn set(&self, key: &str, value: &str) -> Result<()>;
}
