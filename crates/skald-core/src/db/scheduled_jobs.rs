use anyhow::Result;
use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ScheduledJob {
    pub id:                 i64,
    pub title:              String,
    pub description:        String,
    pub cron:               String,
    pub prompt:             String,
    pub agent_id:           String,
    pub session_id:         Option<i64>,
    pub enabled:            bool,
    pub last_run_at:        Option<String>,
    pub next_run_at:        Option<String>,
    pub single_run:         bool,
    pub running_session_id: Option<i64>,
    pub running_since:      Option<String>,
    pub kind:               String,
    pub created_at:         String,
    pub parent_session_id:  Option<i64>,
    pub run_context:        Option<String>,
    pub origin_ref:         Option<String>,
}

const SELECT: &str =
    "SELECT id, title, description, cron, prompt, agent_id, session_id,
            CAST(enabled AS BOOLEAN)    AS enabled,
            last_run_at,
            next_run_at,
            CAST(single_run AS BOOLEAN) AS single_run,
            running_session_id,
            running_since,
            kind,
            created_at,
            parent_session_id,
            run_context,
            origin_ref
     FROM scheduled_jobs";

pub async fn get_by_id(pool: &SqlitePool, id: i64) -> Result<Option<ScheduledJob>> {
    sqlx::query_as::<_, ScheduledJob>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE id = ?")))
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(Into::into)
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<ScheduledJob>> {
    let rows = sqlx::query_as::<_, ScheduledJob>(sqlx::AssertSqlSafe(format!("{SELECT} ORDER BY id")))
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Jobs enabled and due to run: next_run_at is in the past and not currently running.
/// `now_rfc3339` should be `chrono::Utc::now().to_rfc3339()`.
pub async fn list_due(pool: &SqlitePool, now_rfc3339: &str) -> Result<Vec<ScheduledJob>> {
    let rows = sqlx::query_as::<_, ScheduledJob>(sqlx::AssertSqlSafe(format!(
        "{SELECT}
         WHERE kind = 'cron'
           AND enabled = 1
           AND next_run_at IS NOT NULL
           AND next_run_at <= ?
           AND running_session_id IS NULL
         ORDER BY next_run_at",
    )))
    .bind(now_rfc3339)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Jobs that were running when the process was last killed (running_session_id IS NOT NULL).
pub async fn list_interrupted(pool: &SqlitePool) -> Result<Vec<ScheduledJob>> {
    let rows = sqlx::query_as::<_, ScheduledJob>(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE running_session_id IS NOT NULL ORDER BY id",
    )))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn create(
    pool:              &SqlitePool,
    title:             &str,
    description:       &str,
    cron:              &str,
    prompt:            &str,
    agent_id:          &str,
    single_run:        bool,
    next_run_at:       Option<&str>,
    kind:              &str,
    parent_session_id: Option<i64>,
    run_context:       Option<&str>,
    origin_ref:        Option<&str>,
) -> Result<ScheduledJob> {
    let id = sqlx::query(
        "INSERT INTO scheduled_jobs (title, description, cron, prompt, agent_id, single_run, next_run_at, kind, parent_session_id, run_context, origin_ref)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(title)
    .bind(description)
    .bind(cron)
    .bind(prompt)
    .bind(agent_id)
    .bind(single_run as i64)
    .bind(next_run_at)
    .bind(kind)
    .bind(parent_session_id)
    .bind(run_context)
    .bind(origin_ref)
    .execute(pool)
    .await?
    .last_insert_rowid();

    let row = sqlx::query_as::<_, ScheduledJob>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE id = ?")))
        .bind(id)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

pub async fn delete(pool: &SqlitePool, id: i64) -> Result<bool> {
    sqlx::query("DELETE FROM job_runs WHERE job_id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    let n = sqlx::query("DELETE FROM scheduled_jobs WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n > 0)
}

pub async fn set_enabled(pool: &SqlitePool, id: i64, enabled: bool) -> Result<bool> {
    let n = sqlx::query("UPDATE scheduled_jobs SET enabled = ? WHERE id = ?")
        .bind(enabled as i64)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n > 0)
}

/// Update next_run_at without touching anything else (used when re-enabling a job).
pub async fn set_next_run_at(pool: &SqlitePool, id: i64, next_run_at: &str) -> Result<()> {
    sqlx::query("UPDATE scheduled_jobs SET next_run_at = ? WHERE id = ?")
        .bind(next_run_at)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Mark a job as in-flight. Called at the start of run_job(), before handle_message().
pub async fn set_running(pool: &SqlitePool, id: i64, session_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE scheduled_jobs SET running_session_id = ?, running_since = datetime('now') WHERE id = ?",
    )
    .bind(session_id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_run_context(pool: &SqlitePool, id: i64, run_context: Option<&str>) -> Result<bool> {
    let n = sqlx::query("UPDATE scheduled_jobs SET run_context = ? WHERE id = ?")
        .bind(run_context)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n > 0)
}

/// Mark a job as finished. Called at the end of run_job() regardless of outcome.
///
/// - Sets `last_run_at = now`, clears `running_session_id`.
/// - If `next_run_at` is `Some`: updates the field (next scheduled fire).
/// - If `next_run_at` is `None` (single-run job): sets `enabled = 0`.
pub async fn finish_run(
    pool:        &SqlitePool,
    id:          i64,
    next_run_at: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE scheduled_jobs
         SET last_run_at        = datetime('now'),
             running_session_id = NULL,
             running_since      = NULL,
             next_run_at        = COALESCE(?, next_run_at),
             enabled            = CASE WHEN ? IS NULL THEN 0 ELSE enabled END
         WHERE id = ?",
    )
    .bind(next_run_at)
    .bind(next_run_at)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}
