//! Background maintenance task for the `llm_requests` table.
//!
//! Deletes expired metadata rows according to the retention settings in
//! [`LlmRequestsLogConfig`], then `VACUUM`s to reclaim freed pages.
//! Payload/header nulling is gone — those columns moved to `llm_request_payloads`
//! in the owner bucket.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::LlmRequestsLogConfig;

/// Spawns the retention/cleanup loop for the `llm_requests` table.
///
/// First run happens 1 minute after startup, then every 12 hours. The loop exits
/// when `shutdown` is cancelled. Callers should register the returned handle with
/// the task supervisor so it is awaited on shutdown.
pub fn spawn(
    pool: Arc<SqlitePool>,
    cfg: LlmRequestsLogConfig,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        tokio::select! {
            _ = shutdown.cancelled() => { return; }
            _ = tokio::time::sleep(Duration::from_secs(60)) => {}
        }
        loop {
            if let Some(days) = cfg.cleanup_rows_after {
                match super::delete_old_rows(&pool, days).await {
                    Ok(n) if n > 0 => info!(deleted = n, days, "llm_requests: deleted old rows"),
                    Ok(_)  => {}
                    Err(e) => warn!(error = %e, "llm_requests: delete old rows failed"),
                }
            }
            // VACUUM reclaims pages freed by DELETE.
            match sqlx::query("VACUUM").execute(&*pool).await {
                Ok(_)  => info!("llm_requests: VACUUM complete"),
                Err(e) => warn!(error = %e, "llm_requests: VACUUM failed"),
            }
            tokio::select! {
                _ = shutdown.cancelled() => { break; }
                _ = tokio::time::sleep(Duration::from_secs(12 * 3600)) => {}
            }
        }
    })
}
