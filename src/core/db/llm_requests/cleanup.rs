//! Background maintenance task for the `llm_requests` table.
//!
//! Periodically nulls out old payloads/headers and deletes expired rows according
//! to the retention settings in [`LlmRequestsLogConfig`], then `VACUUM`s to reclaim
//! freed pages. Extracted from `Skald::new` so the loop lives next to the queries it
//! calls; the returned handle is registered with the `TaskSupervisor` for shutdown.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::core::config::LlmRequestsLogConfig;

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
            if let Some(days) = cfg.cleanup_request_payload_after {
                match super::null_request_payload(&pool, days).await {
                    Ok(n) if n > 0 => info!(rows = n, days, "llm_requests: nulled request payload"),
                    Ok(_)  => {}
                    Err(e) => warn!(error = %e, "llm_requests: null request payload failed"),
                }
            }
            if let Some(days) = cfg.cleanup_response_payload_after {
                match super::null_response_payload(&pool, days).await {
                    Ok(n) if n > 0 => info!(rows = n, days, "llm_requests: nulled response payload"),
                    Ok(_)  => {}
                    Err(e) => warn!(error = %e, "llm_requests: null response payload failed"),
                }
            }
            if let Some(days) = cfg.cleanup_headers_after {
                match super::null_headers(&pool, days).await {
                    Ok(n) if n > 0 => info!(rows = n, days, "llm_requests: nulled headers"),
                    Ok(_)  => {}
                    Err(e) => warn!(error = %e, "llm_requests: null headers failed"),
                }
            }
            if let Some(days) = cfg.cleanup_rows_after {
                match super::delete_old_rows(&pool, days).await {
                    Ok(n) if n > 0 => info!(deleted = n, days, "llm_requests: deleted old rows"),
                    Ok(_)  => {}
                    Err(e) => warn!(error = %e, "llm_requests: delete old rows failed"),
                }
            }
            // VACUUM reclaims pages freed by DELETE/UPDATE NULL.
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
