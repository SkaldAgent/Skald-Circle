use std::str::FromStr;
use std::sync::Arc;

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use chrono_tz::Tz;
use cron::Schedule;
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tracing::{error, info};

use core_api::system_bus::{SystemEvent, SystemEventBus};

use crate::chat_hub::ChatHub;
use crate::db::chat_sessions;
use crate::db::scheduled_jobs::{self, ScheduledJob};
use crate::session::manager::ChatSessionManager;

pub struct TaskManager {
    pool:       Arc<SqlitePool>,
    tz:         Option<Tz>,
    session:    std::sync::OnceLock<Arc<ChatSessionManager>>,
    hub:        std::sync::OnceLock<Arc<ChatHub>>,
    self_arc:   std::sync::OnceLock<Arc<Self>>,
    system_bus: Arc<SystemEventBus>,
}

/// Returns `(next_utc, is_single)` where `is_single` is `true` when the
/// schedule has no second fire time after the first — i.e. the expression
/// can only ever fire once.  Falls back to system local time when `tz` is `None`.
fn next_fire_and_single(schedule: &Schedule, tz: Option<Tz>) -> Option<(DateTime<Utc>, bool)> {
    if let Some(tz) = tz {
        let mut it = schedule.upcoming(tz);
        let first = it.next()?.with_timezone(&Utc);
        Some((first, it.next().is_none()))
    } else {
        let mut it = schedule.upcoming(Local);
        let first = it.next()?.with_timezone(&Utc);
        Some((first, it.next().is_none()))
    }
}

fn next_fire(schedule: &Schedule, tz: Option<Tz>) -> Option<DateTime<Utc>> {
    next_fire_and_single(schedule, tz).map(|(dt, _)| dt)
}

impl TaskManager {
    pub fn new(pool: Arc<SqlitePool>, tz: Option<Tz>, system_bus: Arc<SystemEventBus>) -> Arc<Self> {
        Arc::new(Self {
            pool,
            tz,
            session:    std::sync::OnceLock::new(),
            hub:        std::sync::OnceLock::new(),
            self_arc:   std::sync::OnceLock::new(),
            system_bus,
        })
    }

    /// The zone cron expressions are evaluated in — the configured `timezone`,
    /// else the system's. Exists so the `execute_task` tool description can name
    /// it instead of hardcoding one: a model told the wrong zone writes a
    /// correct-looking expression that fires at the wrong hour.
    pub fn timezone_name(&self) -> String {
        self.tz
            .map(|tz| tz.name().to_string())
            .or_else(|| iana_time_zone::get_timezone().ok())
            .unwrap_or_else(|| "the server's local timezone".to_string())
    }

    /// Called once after ChatSessionManager is built, breaking the circular dep.
    pub fn set_session(&self, session: Arc<ChatSessionManager>) {
        let _ = self.session.set(session);
    }

    /// Called once after ChatHub is built. Used for completion notifications.
    pub fn set_hub(&self, hub: Arc<ChatHub>) {
        let _ = self.hub.set(hub);
    }

    /// Called once after Arc<Self> is available (in skald.rs after new()).
    pub fn set_self_arc(&self, arc: Arc<Self>) {
        let _ = self.self_arc.set(arc);
    }

    fn session(&self) -> Result<&Arc<ChatSessionManager>> {
        self.session.get().ok_or_else(|| anyhow::anyhow!("cron: session manager not initialized"))
    }

    fn self_arc(&self) -> Result<Arc<Self>> {
        self.self_arc.get().cloned()
            .ok_or_else(|| anyhow::anyhow!("cron: self_arc not initialized"))
    }

    /// Start the background loops. Must be called after set_session().
    /// Returns join handles so the caller can await them during shutdown.
    pub fn start(self: Arc<Self>, shutdown: tokio_util::sync::CancellationToken) -> Vec<tokio::task::JoinHandle<()>> {
        // Main scheduler loop.
        let me = Arc::clone(&self);
        let sd1 = shutdown.clone();
        let h1 = tokio::spawn(async move {
            if let Err(e) = me.recover_interrupted().await {
                error!("cron: startup recovery failed: {e}");
            }
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = sd1.cancelled() => { info!("cron: scheduler loop stopping"); break; }
                    _ = interval.tick() => {
                        if let Err(e) = me.tick().await {
                            error!("cron tick error: {e}");
                        }
                    }
                }
            }
        });

        // Cleanup loop: removes single_run jobs completed more than 7 days ago.
        let pool = Arc::clone(&self.pool);
        let sd2 = shutdown.clone();
        let h2 = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(15)).await;
            let mut interval = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tokio::select! {
                    _ = sd2.cancelled() => { info!("cron: cleanup loop stopping"); break; }
                    _ = interval.tick() => {
                        if let Err(e) = cleanup_expired_single_runs(&pool).await {
                            error!("cron: cleanup error: {e}");
                        }
                    }
                }
            }
        });

        vec![h1, h2]
    }

    async fn recover_interrupted(&self) -> Result<()> {
        let session  = self.session()?;
        let self_arc = self.self_arc()?;
        let jobs = scheduled_jobs::list_interrupted(&self.pool).await?;
        if jobs.is_empty() { return Ok(()); }
        info!("cron: recovering {} interrupted job(s)", jobs.len());
        for job in jobs {
            let pool     = Arc::clone(&self.pool);
            let session  = Arc::clone(session);
            let hub      = self.hub.get().cloned();
            let task_mgr = Arc::clone(&self_arc);
            let tz       = self.tz;
            tokio::spawn(async move {
                if let Err(e) = run_job(&pool, &session, &task_mgr, hub.as_ref(), &job, tz).await {
                    error!("cron: recovery of job {} ('{}') failed: {e}", job.id, job.title);
                }
            });
        }
        Ok(())
    }

    async fn tick(&self) -> Result<()> {
        let session  = self.session()?;
        let self_arc = self.self_arc()?;
        let now  = Utc::now().to_rfc3339();
        let jobs = scheduled_jobs::list_due(&self.pool, &now).await?;
        for job in jobs {
            let pool     = Arc::clone(&self.pool);
            let session  = Arc::clone(session);
            let hub      = self.hub.get().cloned();
            let task_mgr = Arc::clone(&self_arc);
            let job      = job.clone();
            let tz       = self.tz;
            tokio::spawn(async move {
                if let Err(e) = run_job(&pool, &session, &task_mgr, hub.as_ref(), &job, tz).await {
                    error!("cron job {} ('{}') failed: {e}", job.id, job.title);
                }
            });
        }
        Ok(())
    }

    // ── Sync wrappers (called from LLM tools via block_in_place) ─────────────

    pub fn list_jobs(&self) -> Result<Vec<ScheduledJob>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(scheduled_jobs::list(&self.pool))
        })
    }

    /// Validate that `agent_id` names a runnable task agent (non-empty, exists,
    /// `type == Task`). Single gate shared by every job-creation entry point so
    /// cron / sync / async / project-ticket paths all agree — no silent default.
    fn require_task_agent(agent_id: &str) -> Result<()> {
        if agent_id.trim().is_empty() {
            anyhow::bail!("agent_id is required — specify which task agent runs this task (no default)");
        }
        crate::agents::load_task_meta(agent_id)?;
        Ok(())
    }

    pub fn add_job(
        &self,
        title:             &str,
        description:       &str,
        cron:              &str,
        prompt:            &str,
        agent_id:          &str,
        single_run:        bool,
        kind:              &str,
        parent_session_id: Option<i64>,
        run_context:       Option<&str>,
    ) -> Result<ScheduledJob> {
        Self::require_task_agent(agent_id)?;
        let (first_fire, _is_single, single_run) = if kind == "sync" || kind == "immediate" {
            (None, true, true)
        } else {
            let schedule = Schedule::from_str(cron).map_err(|_| {
                anyhow::anyhow!(
                    "Invalid cron expression: '{cron}'. Use 7-field format: \
                     sec min hour dom month dow year  (e.g. '0 0 9 * * * *' = every day at 9:00)"
                )
            })?;
            let (first, single) = next_fire_and_single(&schedule, self.tz)
                .ok_or_else(|| anyhow::anyhow!("Cron expression '{cron}' has no upcoming fire times"))?;
            let single_run = single_run || single;
            (Some(first.to_rfc3339()), single, single_run)
        };
        let next_run_at: Option<&str> = first_fire.as_deref();
        let job = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(scheduled_jobs::create(
                &self.pool, title, description, cron, prompt, agent_id,
                single_run, next_run_at, kind, parent_session_id, run_context, None,
            ))
        })?;
        Ok(job)
    }

    /// Execute a task synchronously: creates the DB record, runs it inline,
    /// and returns the agent's final response. Blocks until completion.
    pub fn add_job_sync(
        &self,
        title:          &str,
        description:    &str,
        prompt:         &str,
        agent_id:       &str,
        run_context: Option<&str>,
    ) -> Result<String> {
        Self::require_task_agent(agent_id)?;
        let job = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(scheduled_jobs::create(
                &self.pool, title, description, "", prompt, agent_id,
                true, None, "sync", None, run_context, None,
            ))
        })?;
        let session  = self.session()?;
        let self_arc = self.self_arc()?;
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(
                run_job(&self.pool, session, &self_arc, self.hub.get(), &job, self.tz)
            )
        })?;
        Ok(result.unwrap_or_else(|| "(no output)".to_string()))
    }

    /// Start a task asynchronously: creates the DB record, spawns the run,
    /// returns immediately. Result is injected into parent_session_id when done.
    pub fn add_job_async(
        &self,
        title:             &str,
        description:       &str,
        prompt:            &str,
        agent_id:          &str,
        parent_session_id: i64,
        run_context:       Option<&str>,
    ) -> Result<ScheduledJob> {
        Self::require_task_agent(agent_id)?;
        let job = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(scheduled_jobs::create(
                &self.pool, title, description, "", prompt, agent_id,
                true, None, "async", Some(parent_session_id), run_context, None,
            ))
        })?;
        let pool     = Arc::clone(&self.pool);
        let session  = self.session()?.clone();
        let hub      = self.hub.get().cloned();
        let task_mgr = self.self_arc()?;
        let tz       = self.tz;
        let job_c    = job.clone();
        tokio::spawn(async move {
            if let Err(e) = run_job(&pool, &session, &task_mgr, hub.as_ref(), &job_c, tz).await {
                error!("async task {} ('{}') failed: {e}", job_c.id, job_c.title);
            }
        });
        Ok(job)
    }

    /// Create and immediately spawn an async job with an opaque `origin_ref`.
    /// Returns the created `ScheduledJob` (caller uses its `id` for tracking).
    /// Unlike `add_job_async`, no `parent_session_id` is set — completion is
    /// delivered via `SystemEvent::JobCompleted` on the system bus.
    pub fn spawn_async_job(
        &self,
        title:       &str,
        description: &str,
        prompt:      &str,
        agent_id:    &str,
        run_context: Option<&str>,
        origin_ref:  &str,
    ) -> Result<scheduled_jobs::ScheduledJob> {
        Self::require_task_agent(agent_id)?;
        let job = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(scheduled_jobs::create(
                &self.pool, title, description, "", prompt, agent_id,
                true, None, "async", None, run_context, Some(origin_ref),
            ))
        })?;
        let pool     = Arc::clone(&self.pool);
        let session  = self.session()?.clone();
        let hub      = self.hub.get().cloned();
        let task_mgr = self.self_arc()?;
        let tz       = self.tz;
        let job_c    = job.clone();
        tokio::spawn(async move {
            if let Err(e) = run_job(&pool, &session, &task_mgr, hub.as_ref(), &job_c, tz).await {
                error!("project-ticket job {} failed: {e}", job_c.id);
            }
        });
        Ok(job)
    }

    pub fn delete_job(&self, id: i64) -> Result<bool> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(scheduled_jobs::delete(&self.pool, id))
        })
    }

    pub fn toggle_job(&self, id: i64, enabled: bool) -> Result<bool> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let found = scheduled_jobs::set_enabled(&self.pool, id, enabled).await?;
                if found && enabled {
                    // Recalculate next_run_at when re-enabling so a stale timestamp
                    // doesn't cause an immediate spurious fire.
                    let jobs = scheduled_jobs::list(&self.pool).await?;
                    if let Some(job) = jobs.iter().find(|j| j.id == id) {
                        let tz = self.tz;
                        if let Some(next) = Schedule::from_str(&job.cron)
                            .ok()
                            .and_then(|s| next_fire(&s, tz))
                            .map(|t| t.to_rfc3339())
                        {
                            scheduled_jobs::set_next_run_at(&self.pool, id, &next).await?;
                        }
                    }
                }
                Ok(found)
            })
        })
    }
}

// ── Job execution ─────────────────────────────────────────────────────────────

async fn run_job(
    pool:     &SqlitePool,
    session:  &ChatSessionManager,
    task_mgr: &Arc<TaskManager>,
    hub:      Option<&Arc<ChatHub>>,
    job:      &ScheduledJob,
    tz:       Option<Tz>,
) -> Result<Option<String>> {
    info!("running {} task {} ('{}')", job.kind, job.id, job.title);

    let started_at = Utc::now();

    let (session_id, _) = session.create_session(&job.agent_id, "cron", false, true, None).await?;
    scheduled_jobs::set_running(pool, job.id, session_id).await?;

    if let Some(rc) = &job.run_context {
        chat_sessions::set_run_context(pool, session_id, Some(rc.as_str())).await.ok();
    }

    let handler = session.get_or_create_handler(session_id).await?;
    handler.set_context_label(format!("CronJob: {}", job.title));
    if job.kind == "async" {
        if let Some(parent_id) = job.parent_session_id {
            handler.set_scratchpad_session_id(parent_id);
        }
    }

    let job_context = format!(
        "[Job context]\nJob ID: {} — {}\nTime: {} UTC",
        job.id, job.title,
        started_at.format("%Y-%m-%d %H:%M"),
    );

    // Build interface_tools: execute_subtask for background sessions (sync only, no async/cron).
    let task_mgr_clone = Arc::clone(task_mgr);
    let execute_subtask_tool = build_execute_subtask_tool(task_mgr_clone, job.run_context.clone());

    // Use a large buffer and drain rx concurrently with handle_message to avoid
    // deadlock: handle_message may emit many events (ToolStart/ToolDone/Thinking
    // per tool call), and the channel blocks when full if nobody is reading.
    let (tx, mut rx) = mpsc::channel(512);

    let handler_arc = Arc::clone(&handler);
    let prompt      = job.prompt.clone();
    let ctx         = job_context.clone();
    let jh = tokio::spawn(async move {
        handler_arc.handle_message(
            &prompt,
            None,
            None,
            Some(ctx),
            None,
            vec![execute_subtask_tool],
            std::collections::HashMap::new(),
            tx,
            false,
            None,
            None, // non-interactive: no live user-message injection
        ).await
    });

    // Drain events concurrently. rx closes when the last tx clone is dropped,
    // which happens only after the turn completes the full sub-agent chain.
    while let Some(_) = rx.recv().await {}

    let handle_result = jh.await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("run_job task panicked: {e}")));

    let completed_at  = Utc::now();
    let duration_ms   = (completed_at - started_at).num_milliseconds();
    let final_response = last_assistant_message(pool, session_id).await.ok().flatten();

    let next_run_at: Option<String> = if job.single_run || job.kind != "cron" {
        None
    } else {
        Schedule::from_str(&job.cron).ok()
            .and_then(|s| next_fire(&s, tz))
            .map(|t| t.to_rfc3339())
    };

    match handle_result {
        Ok(_) => {
            record_job_run(pool, job.id, session_id, &started_at.to_rfc3339(),
                           &completed_at.to_rfc3339(), duration_ms,
                           "completed", final_response.as_deref(), None).await?;
            scheduled_jobs::finish_run(pool, job.id, next_run_at.as_deref()).await?;

            task_mgr.system_bus.send(SystemEvent::JobCompleted {
                job_id:     job.id,
                origin_ref: job.origin_ref.clone(),
                result:     final_response.clone(),
                error:      None,
            });

            match job.kind.as_str() {
                "cron" => {
                    if let Some(hub) = hub {
                        let outcome = final_response.as_deref().unwrap_or("(no output)");
                        hub.notify(crate::notification::Notification {
                            source:     "cron".into(),
                            event_type: "cron_result".into(),
                            summary:    format!(
                                "Cron job \"{}\" (ID {}) completed: {}",
                                job.title, job.id, outcome,
                            ),
                            event_time: Utc::now().to_rfc3339(),
                            refs:       serde_json::json!({ "job_id": job.id, "title": job.title }),
                        }).await.ok();
                    }
                }
                "async" => {
                    if let Some(parent_id) = job.parent_session_id {
                        if let Some(hub) = hub {
                            inject_async_result(
                                &task_mgr.pool,
                                hub,
                                parent_id,
                                job.id,
                                &job.title,
                                final_response.as_deref().unwrap_or("(no output)"),
                            ).await;
                        }
                    }
                }
                _ => {} // sync: result was already returned inline via add_job_sync
            }

            info!("{} task {} done", job.kind, job.id);
            Ok(final_response)
        }
        Err(e) => {
            let err_str = e.to_string();
            record_job_run(pool, job.id, session_id, &started_at.to_rfc3339(),
                           &completed_at.to_rfc3339(), duration_ms,
                           "failed", None, Some(&err_str)).await?;
            scheduled_jobs::finish_run(pool, job.id, next_run_at.as_deref()).await?;

            task_mgr.system_bus.send(SystemEvent::JobCompleted {
                job_id:     job.id,
                origin_ref: job.origin_ref.clone(),
                result:     None,
                error:      Some(err_str.clone()),
            });

            if let Some(hub) = hub {
                hub.notify(crate::notification::Notification {
                    source:     "cron".into(),
                    event_type: "cron_error".into(),
                    summary:    format!(
                        "Cron job \"{}\" (ID {}) failed: {} (check the logs)",
                        job.title, job.id, err_str,
                    ),
                    event_time: Utc::now().to_rfc3339(),
                    refs:       serde_json::json!({ "job_id": job.id, "title": job.title }),
                }).await.ok();
            }
            Err(e)
        }
    }
}

/// Delivers an async task's result to the parent session through the loop's
/// [`AsyncResultSink`] seam (blueprint §7.2): the library writes the synthetic
/// assistant message + completed `task_completed` call, and Skald's
/// [`DurableSink`] resumes the parent so the model reads it right away.
///
/// Failures are logged, never propagated: the job itself succeeded, and losing
/// the delivery must not mark it failed.
async fn inject_async_result(
    pool:              &Arc<SqlitePool>,
    hub:               &Arc<ChatHub>,
    parent_session_id: i64,
    task_id:           i64,
    task_title:        &str,
    result:            &str,
) {
    use agent_loop::delegate::{AsyncResultSink, CompletedTask};
    use crate::loop_adapters::async_task::DurableSink;
    use crate::loop_adapters::history::SqliteHistory;

    info!(parent_session_id, task_id, task_title, "delivering async task result");

    let sink = DurableSink::new(Arc::clone(pool), Arc::clone(hub));
    let delivered = sink
        .deliver(SqliteHistory::conversation(parent_session_id), CompletedTask {
            id:     agent_loop::ids::TaskId(task_id),
            title:  task_title.to_string(),
            result: result.to_string(),
        })
        .await;
    if let Err(e) = delivered {
        error!(parent_session_id, task_id, "async result delivery failed: {e}");
    }
}

/// Builds the `execute_subtask` InterfaceTool injected into background sessions.
/// Background tasks can only run synchronous sub-tasks — no cron or async.
fn build_execute_subtask_tool(task_mgr: Arc<TaskManager>, run_context: Option<String>) -> crate::session::handler::InterfaceTool {
    use crate::session::handler::{InterfaceTool, ToolFuture};
    use serde_json::json;

    InterfaceTool {
        definition: json!({
            "type": "function",
            "function": {
                "name": crate::tools::tool_names::EXECUTE_SUBTASK,
                "description": "Run a synchronous sub-task and return its result. Blocks until the sub-task completes.",
                "parameters": {
                    "type": "object",
                    "required": ["title", "prompt", "agent_id"],
                    "properties": {
                        "title":       { "type": "string", "description": "Short name for this sub-task" },
                        "description": { "type": "string", "description": "What this sub-task does" },
                        "prompt":      { "type": "string", "description": "Prompt sent to the agent" },
                        "agent_id":    { "type": "string", "description": "Task agent to run (required; e.g. software-engineer, researcher, generalist)" }
                    }
                }
            }
        }),
        handler: Arc::new(move |args: serde_json::Value| -> ToolFuture {
            let tm             = Arc::clone(&task_mgr);
            let title          = args["title"].as_str().unwrap_or("").to_string();
            let desc           = args["description"].as_str().unwrap_or("").to_string();
            let prompt         = args["prompt"].as_str().unwrap_or("").to_string();
            let agent_id       = args["agent_id"].as_str().unwrap_or("").to_string();
            let run_context = run_context.clone();
            Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    tm.add_job_sync(&title, &desc, &prompt, &agent_id, run_context.as_deref())
                })
                .await
                .map_err(|e| anyhow::anyhow!("execute_subtask task panicked: {e}"))?
            })
        }),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn record_job_run(
    pool:         &SqlitePool,
    job_id:       i64,
    session_id:   i64,
    started_at:   &str,
    completed_at: &str,
    duration_ms:  i64,
    status:       &str,
    response:     Option<&str>,
    error:        Option<&str>,
) -> Result<()> {
    crate::db::job_runs::insert(
        pool, job_id, Some(session_id),
        started_at, completed_at, duration_ms,
        status, response, error,
    ).await.map(|_| ())
}

/// Returns the most recent successful assistant message in the given session.
async fn last_assistant_message(pool: &SqlitePool, session_id: i64) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT ch.content
         FROM   chat_history ch
         JOIN   chat_sessions_stack css ON ch.session_stack_id = css.id
         WHERE  css.session_id = ? AND ch.role = 'assistant' AND ch.status = 'ok'
         ORDER  BY ch.id DESC
         LIMIT  1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(c,)| c))
}

async fn cleanup_expired_single_runs(pool: &SqlitePool) -> Result<()> {
    // The set of jobs about to be deleted, reused by each cascade step below.
    const EXPIRED: &str = "SELECT id FROM scheduled_jobs
                           WHERE single_run = 1
                             AND enabled    = 0
                             AND last_run_at < datetime('now', '-7 days')";

    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM job_runs WHERE job_id IN ({EXPIRED})"
    )))
    .execute(pool)
    .await?;

    let n = sqlx::query(
        "DELETE FROM scheduled_jobs
         WHERE single_run = 1
           AND enabled    = 0
           AND last_run_at < datetime('now', '-7 days')",
    )
    .execute(pool)
    .await?
    .rows_affected();
    if n > 0 {
        info!("cron: removed {n} expired single-run job(s)");
    }
    Ok(())
}
