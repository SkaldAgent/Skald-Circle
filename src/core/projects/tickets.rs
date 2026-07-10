use std::sync::Arc;

use anyhow::{Result, anyhow};
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use core_api::system_bus::{SystemEvent, SystemEventBus};

use crate::core::cron::TaskManager;
use crate::core::db::{project_tickets, project_tickets::ProjectTicket, projects};
use crate::core::run_context::RunContext;

pub struct ProjectTicketManager {
    db:       Arc<SqlitePool>,
    task_mgr: std::sync::OnceLock<Arc<TaskManager>>,
}

impl ProjectTicketManager {
    pub fn new(db: Arc<SqlitePool>) -> Arc<Self> {
        Arc::new(Self {
            db,
            task_mgr: std::sync::OnceLock::new(),
        })
    }

    pub fn set_task_manager(&self, tm: Arc<TaskManager>) {
        let _ = self.task_mgr.set(tm);
    }

    /// Subscribe to the system bus and react to `JobCompleted` events whose
    /// `origin_ref` starts with `"PROJECT_TASK:"`. Spawns a background task.
    pub fn start_listener(
        self: Arc<Self>,
        system_bus: Arc<SystemEventBus>,
        shutdown:   CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut rx = system_bus.subscribe();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    res = rx.recv() => {
                        match res {
                            Ok(SystemEvent::JobCompleted { origin_ref: Some(ref s), result, error, .. })
                                if s.starts_with("PROJECT_TASK:") =>
                            {
                                if let Some(tid) = s.strip_prefix("PROJECT_TASK:")
                                    .and_then(|n| n.parse::<i64>().ok())
                                {
                                    if let Err(e) = self.on_job_completed(
                                        tid,
                                        result.as_deref(),
                                        error.as_deref(),
                                    ).await {
                                        warn!(error = %e, ticket_id = tid, "ticket completion failed");
                                    }
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!("ProjectTicketManager: system_bus lagged by {n} events");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                            _ => {}
                        }
                    }
                }
            }
        })
    }

    // ── CRUD ─────────────────────────────────────────────────────────────────

    pub async fn list(&self, project_id: i64) -> Result<Vec<ProjectTicket>> {
        project_tickets::list_for_project(&self.db, project_id).await
    }

    pub async fn get(&self, id: i64) -> Result<Option<ProjectTicket>> {
        project_tickets::get(&self.db, id).await
    }

    pub async fn create(
        &self,
        project_id:  i64,
        title:       &str,
        description: &str,
        agent_id:    &str,
        run_context: Option<&RunContext>,
    ) -> Result<ProjectTicket> {
        let rc_json = run_context.map(|rc| rc.to_db());
        let ticket = project_tickets::create(
            &self.db, project_id, title, description, agent_id, rc_json.as_deref(),
        ).await?;
        projects::touch(&self.db, project_id).await?;
        Ok(ticket)
    }

    pub async fn delete(&self, id: i64) -> Result<bool> {
        let ticket = project_tickets::get(&self.db, id).await?;
        let found  = project_tickets::delete(&self.db, id).await?;
        if found {
            if let Some(t) = ticket {
                projects::touch(&self.db, t.project_id).await?;
            }
        }
        Ok(found)
    }

    // ── Lifecycle ─────────────────────────────────────────────────────────────

    /// Builds a runtime RunContext and starts the ticket as a background job.
    ///
    /// The stored RC (ticket → project) carries only static config set at creation
    /// time (e.g. `security_group`). All runtime fields are computed here:
    /// - `working_directory` — always set to `project.path`
    /// - `allow_fs_writes`  — project tree + Skald's own `data/` directory
    /// - `system_prompt`    — project context fragments prepended before any stored ones
    pub async fn start(&self, ticket_id: i64) -> Result<()> {
        let task_mgr = self.task_mgr.get()
            .ok_or_else(|| anyhow!("ProjectTicketManager: task_manager not initialized"))?;

        let ticket  = project_tickets::get(&self.db, ticket_id).await?
            .ok_or_else(|| anyhow!("ticket {ticket_id} not found"))?;
        let project = projects::get(&self.db, ticket.project_id).await?
            .ok_or_else(|| anyhow!("project {} not found", ticket.project_id))?;

        // Resolve base RC (ticket override → project default → empty), then layer the
        // project-runtime fields (WD, fs-write grants, project-context system prompt).
        // The stored RC carries only static config (e.g. security_group set at creation).
        let base: Option<RunContext> =
            ticket.run_context.as_deref().and_then(RunContext::from_db)
            .or_else(|| project.run_context.as_deref().and_then(RunContext::from_db));
        let rc = super::build_runtime_run_context(&project, base);

        let origin_ref = format!("PROJECT_TASK:{ticket_id}");
        let rc_json    = rc.to_db();

        let job = task_mgr.spawn_async_job(
            &ticket.title,
            &ticket.description,
            &ticket.description,
            &ticket.agent_id,
            Some(&rc_json),
            &origin_ref,
        )?;

        project_tickets::start(&self.db, ticket_id, job.id).await?;
        projects::touch(&self.db, ticket.project_id).await?;
        Ok(())
    }

    /// Called when a `SystemEvent::JobCompleted` with matching `origin_ref` is received.
    async fn on_job_completed(
        &self,
        ticket_id: i64,
        result:    Option<&str>,
        error:     Option<&str>,
    ) -> Result<()> {
        let project_id = project_tickets::get(&self.db, ticket_id).await?
            .map(|t| t.project_id);
        project_tickets::complete(&self.db, ticket_id, result, error).await?;
        if let Some(pid) = project_id {
            projects::touch(&self.db, pid).await?;
        }
        Ok(())
    }

    /// Reset a ticket back to todo, clearing all run state.
    pub async fn reset(&self, ticket_id: i64) -> Result<()> {
        let project_id = project_tickets::get(&self.db, ticket_id).await?
            .map(|t| t.project_id);
        project_tickets::reset(&self.db, ticket_id).await?;
        if let Some(pid) = project_id {
            projects::touch(&self.db, pid).await?;
        }
        Ok(())
    }
}
