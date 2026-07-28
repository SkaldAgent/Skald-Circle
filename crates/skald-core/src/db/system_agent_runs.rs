//! Execution log of the system agents (blueprint §13).
//!
//! One row per run, in the **user's own** database — a system agent runs on a
//! user's behalf, over their events, so its trace is theirs (see the table
//! comment in [`super::create_owner_tables`]). There is no `user_id` column
//! because the file is the owner.
//!
//! The write is split in two, unlike [`super::job_runs`]: [`start`] before the
//! agent runs, [`finish`] after. A run that never reaches `finish` — the process
//! died mid-turn — stays `running` and is swept to `failed` by the next [`start`]
//! for the same agent, which is safe because the scheduler is sequential and
//! single-instance: no live run can be in that state when a new one begins.

use anyhow::Result;
use sqlx::SqlitePool;

/// Terminal statuses. `running` is the transient one written by [`start`].
pub const STATUS_COMPLETED: &str = "completed";
pub const STATUS_FAILED:    &str = "failed";
pub const STATUS_CANCELLED: &str = "cancelled";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SystemAgentRun {
    pub id:           i64,
    pub agent_id:     String,
    pub session_id:   Option<i64>,
    pub started_at:   String,
    pub completed_at: Option<String>,
    pub duration_ms:  Option<i64>,
    pub status:       String,
    /// Free-form JSON with the agent's own counters (event triage: events processed,
    /// notifications emitted). Never the event contents.
    pub stats:        Option<String>,
    pub error:        Option<String>,
    pub created_at:   String,
}

/// Open a run: sweep any leftover `running` row for this agent, then insert.
pub async fn start(pool: &SqlitePool, agent_id: &str) -> Result<i64> {
    sqlx::query(
        "UPDATE system_agent_runs
         SET status = 'failed', error = 'interrupted (server restarted)',
             completed_at = datetime('now')
         WHERE agent_id = ? AND status = 'running'",
    )
    .bind(agent_id)
    .execute(pool)
    .await?;

    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO system_agent_runs (agent_id, started_at, status)
         VALUES (?, datetime('now'), 'running')
         RETURNING id",
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Close a run. `duration_ms` is computed by the caller, which holds the
/// `Instant` — `datetime('now')` has second resolution and a tick is often faster.
pub async fn finish(
    pool:        &SqlitePool,
    run_id:      i64,
    status:      &str,
    session_id:  Option<i64>,
    duration_ms: i64,
    stats:       Option<&str>,
    error:       Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE system_agent_runs
         SET status = ?, session_id = ?, completed_at = datetime('now'),
             duration_ms = ?, stats = ?, error = ?
         WHERE id = ?",
    )
    .bind(status)
    .bind(session_id)
    .bind(duration_ms)
    .bind(stats)
    .bind(error)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Newest-first page of runs, optionally narrowed to one agent.
pub async fn list(
    pool:     &SqlitePool,
    agent_id: Option<&str>,
    limit:    i64,
    offset:   i64,
) -> Result<Vec<SystemAgentRun>> {
    let rows = sqlx::query_as::<_, SystemAgentRun>(
        "SELECT id, agent_id, session_id, started_at, completed_at, duration_ms,
                status, stats, error, created_at
         FROM   system_agent_runs
         WHERE  (? IS NULL OR agent_id = ?)
         ORDER  BY id DESC
         LIMIT ? OFFSET ?",
    )
    .bind(agent_id)
    .bind(agent_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Total matching [`list`]'s filter, for pagination.
pub async fn count(pool: &SqlitePool, agent_id: Option<&str>) -> Result<i64> {
    let total = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM system_agent_runs WHERE (? IS NULL OR agent_id = ?)",
    )
    .bind(agent_id)
    .bind(agent_id)
    .fetch_one(pool)
    .await?;
    Ok(total)
}
