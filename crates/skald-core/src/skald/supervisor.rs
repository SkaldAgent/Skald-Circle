//! Background-task supervision.
//!
//! Every long-lived task spawned during `Skald::new` is registered here by name so
//! that `Skald::shutdown` can join them all against a single deadline and report any
//! laggards individually. This replaces the previous `bg_handles` vec, which only
//! tracked a subset of the spawned tasks (leaving the log-cleanup loop and
//! `mcp.initialize` fire-and-forget and never awaited).

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::warn;

/// Tracks named background-task handles for graceful shutdown.
pub struct TaskSupervisor {
    handles: Mutex<Vec<(&'static str, JoinHandle<()>)>>,
}

impl TaskSupervisor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { handles: Mutex::new(Vec::new()) })
    }

    /// Spawn a named future and track its handle.
    pub fn spawn<F>(&self, name: &'static str, fut: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.handles.lock().unwrap().push((name, tokio::spawn(fut)));
    }

    /// Adopt an already-spawned handle (for managers whose `start()` returns one).
    pub fn adopt_one(&self, name: &'static str, handle: JoinHandle<()>) {
        self.handles.lock().unwrap().push((name, handle));
    }

    /// Adopt a batch of handles (e.g. `cron.start()` returns `Vec<JoinHandle<()>>`).
    pub fn adopt(&self, name: &'static str, handles: Vec<JoinHandle<()>>) {
        let mut guard = self.handles.lock().unwrap();
        for h in handles {
            guard.push((name, h));
        }
    }

    /// Join all tracked tasks against a shared deadline, logging any that do not
    /// finish in time by name. Dropping a timed-out `JoinHandle` does not abort the
    /// task; every task is already signalled via the shutdown `CancellationToken`.
    pub async fn join_all(&self, timeout: Duration) {
        let handles = std::mem::take(&mut *self.handles.lock().unwrap());
        let deadline = tokio::time::Instant::now() + timeout;
        for (name, handle) in handles {
            if tokio::time::timeout_at(deadline, handle).await.is_err() {
                warn!(task = name, "background task did not finish within shutdown deadline");
            }
        }
    }
}
