//! Skald's async delegation seam (blueprint §7.2) — `execute_task mode=async`.
//!
//! The library defines *what* an out-of-band task is ([`AsyncExecutor`] submits
//! it, [`AsyncResultSink`] delivers its result); this says *how* Skald runs one:
//!
//! - [`CronExecutor`] — a row in `scheduled_jobs`, run by the cron machinery.
//!   Durable by construction: the row survives a restart and `recover_interrupted`
//!   re-runs a job that was in flight when the process died. That is the whole
//!   reason Skald does not use the crate's `InProcessExecutor`, which is lossy.
//! - [`DurableSink`] — the crate's store write plus Skald's wake-up: the result
//!   is history the instant it lands, and the parent session is resumed so the
//!   model actually reads it.
//!
//! The `TaskManager` arrives late (it needs a `ChatSessionManager`, which builds
//! the loop runtime — the same cycle `ChatHub` resolves with its own
//! `OnceLock`), so the executor is constructed empty and filled in at wiring
//! time. Submitting before that is a wiring bug and says so.

use std::sync::{Arc, OnceLock};

use agent_loop::delegate::{
    AsyncExecutor, AsyncResultSink, AsyncSpec, CompletedTask, StoreSink, TaskHandle,
};
use agent_loop::ids::{ConversationId, TaskId};
use agent_loop::store::HistoryStore;
use sqlx::SqlitePool;

use crate::chat_hub::ChatHub;
use crate::cron::TaskManager;
use crate::loop_adapters::history::SqliteHistory;
use crate::loop_adapters::scope::TurnScope;

// ── CronExecutor ─────────────────────────────────────────────────────────────

/// Runs a delegated task as a `scheduled_jobs` row of kind `async`.
pub struct CronExecutor {
    tasks: OnceLock<Arc<TaskManager>>,
}

impl CronExecutor {
    pub fn new() -> Self {
        Self { tasks: OnceLock::new() }
    }

    /// Called once at wiring time (see the module docs). A second call is
    /// ignored — the first manager is the one the user's jobs belong to.
    pub fn set_task_manager(&self, tasks: Arc<TaskManager>) {
        let _ = self.tasks.set(tasks);
    }
}

impl Default for CronExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[agent_loop::async_trait]
impl AsyncExecutor for CronExecutor {
    async fn submit(&self, spec: AsyncSpec) -> agent_loop::Result<TaskHandle> {
        let tasks = self
            .tasks
            .get()
            .ok_or_else(|| anyhow::anyhow!("async tasks are not available in this session"))?;
        let session_id = SqliteHistory::session_id(&spec.conversation)?;

        // The child inherits the parent's run context (security group, project
        // root): a background task must not run with more reach than the turn
        // that asked for it.
        let run_context = match TurnScope::from(&spec.extensions) {
            Some(scope) => scope.run_context.read().await.as_ref().map(|rc| rc.to_db()),
            None        => None,
        };

        let title = spec
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| format!("{} task", spec.agent));
        let description = spec.description.clone().unwrap_or_default();

        let job = tasks.add_job_async(
            &title,
            &description,
            &spec.prompt,
            &spec.agent,
            session_id,
            run_context.as_deref(),
        )?;
        Ok(TaskHandle { id: TaskId(job.id), title: job.title })
    }
}

// ── DurableSink ──────────────────────────────────────────────────────────────

/// Delivers a finished task into its parent conversation: the crate writes the
/// synthetic assistant message + completed call, then the parent session is
/// resumed so the model reads the result now rather than on its next message.
///
/// `ChatHub::resume` skips a session with a turn already in flight, which is the
/// right rule here too: a live loop reads the store each round and picks the
/// result up on its own. The wake-up addresses the parent **by session id**,
/// never by source: one source may now carry several conversations (secondary
/// tabs) or have moved to a fresh one since the task started, and resuming the
/// source's active session would run the recovery on the wrong conversation —
/// a silent no-op there, while this result sat unread until the next message.
pub struct DurableSink {
    inner: StoreSink,
    hub:   Arc<ChatHub>,
}

impl DurableSink {
    pub fn new(pool: Arc<SqlitePool>, hub: Arc<ChatHub>) -> Self {
        let store: Arc<dyn HistoryStore> = Arc::new(SqliteHistory::new(pool));
        Self { inner: StoreSink::new(store), hub }
    }
}

#[agent_loop::async_trait]
impl AsyncResultSink for DurableSink {
    async fn deliver(&self, parent: ConversationId, task: CompletedTask) -> agent_loop::Result<()> {
        self.inner.deliver(parent.clone(), task).await?;

        let session_id = SqliteHistory::session_id(&parent)?;
        self.hub.resume_for_session(session_id).await
    }
}
