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

/// One background (`async`) task as the conversation that started it sees it.
/// A flattened join of the job with its latest run — the chat cares about a
/// task's *current* state, not its scheduling row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SessionTask {
    pub job_id:     i64,
    pub title:      String,
    pub agent_id:   String,
    /// The task's own session (`#session/{id}`).
    pub session_id: Option<i64>,
    /// `running` or `failed` — the only two states this query returns.
    pub state:      String,
    pub error:      Option<String>,
    /// When it started, normalised to RFC 3339 (see [`normalise_ts`]).
    pub started_at: Option<String>,
}

/// The background tasks one conversation should still be showing: everything
/// running right now, plus failures from the last `failed_within_minutes`.
///
/// Those are the two states a person can still act on — and the reason this
/// query exists at all is the browser reload: the strip is driven by
/// `ServerEvent::TaskUpdate`, which is a live broadcast with no replay, so
/// without a load-time read a refresh would empty a chat that still has work
/// running under it. Successes are deliberately absent: a completed task's
/// result is already a message in the conversation, which is a better place to
/// read it than a status chip.
pub async fn list_for_parent_session(
    pool:                  &SqlitePool,
    parent_session_id:     i64,
    failed_within_minutes: i64,
) -> Result<Vec<SessionTask>> {
    let rows = sqlx::query_as::<_, SessionTask>(
        "SELECT sj.id                                          AS job_id,
                sj.title                                       AS title,
                sj.agent_id                                    AS agent_id,
                COALESCE(sj.running_session_id, jr.session_id) AS session_id,
                CASE WHEN sj.running_session_id IS NOT NULL
                     THEN 'running' ELSE jr.status END         AS state,
                jr.error                                       AS error,
                COALESCE(sj.running_since, jr.started_at)      AS started_at
         FROM   scheduled_jobs sj
         LEFT   JOIN job_runs jr
                ON jr.id = (SELECT id FROM job_runs
                            WHERE job_id = sj.id ORDER BY id DESC LIMIT 1)
         WHERE  sj.kind = 'async'
           AND  sj.parent_session_id = ?
           AND  (sj.running_session_id IS NOT NULL
                 -- `datetime()` on both sides, never a raw string compare:
                 -- `completed_at` is RFC 3339 (`…T…+00:00`) and the cutoff is
                 -- SQLite-shaped, and `'T' > ' '` makes every same-day row
                 -- compare as newer than the cutoff — a window that lets
                 -- through everything it was meant to exclude.
                 OR (jr.status = 'failed'
                     AND datetime(jr.completed_at) >= datetime('now', ?)))
         ORDER  BY sj.id",
    )
    .bind(parent_session_id)
    .bind(format!("-{failed_within_minutes} minutes"))
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter()
        .map(|mut t| { t.started_at = t.started_at.as_deref().and_then(normalise_ts); t })
        .collect())
}

/// The two timestamp shapes this table mixes, as one RFC 3339 string:
/// `running_since` is written by SQLite's `datetime('now')` (`Y-m-d H:M:S`,
/// UTC, no offset) while `job_runs.started_at` is already RFC 3339. A client
/// that guesses wrong is off by its own timezone, so the guess is made here.
fn normalise_ts(raw: &str) -> Option<String> {
    use chrono::{DateTime, NaiveDateTime, Utc};
    DateTime::parse_from_rfc3339(raw)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|n| n.and_utc())
        })
        .map(|d| d.to_rfc3339())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One conversation (session 1) with a background task in each state, plus
    /// the rows the query must not pick up: another conversation's task, and a
    /// cron job (which belongs to nobody's chat).
    async fn seeded() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::create_owner_tables(&pool).await.unwrap();

        let q = |sql: &'static str| sqlx::query(sql).execute(&pool);
        q("INSERT INTO chat_sessions (id, title, source) VALUES (1, 'chat', 'web')").await.unwrap();
        q("INSERT INTO chat_sessions (id, title, source) VALUES (2, 'other', 'mobile')").await.unwrap();

        let job = |id: i64, title: &'static str, kind: &'static str,
                   parent: Option<i64>, running: Option<i64>| {
            sqlx::query(
                "INSERT INTO scheduled_jobs
                    (id, title, cron, prompt, agent_id, kind, parent_session_id,
                     running_session_id, running_since, single_run)
                 VALUES (?, ?, '', 'p', 'researcher', ?, ?, ?, '2026-08-04 10:00:00', 1)",
            )
            .bind(id).bind(title).bind(kind).bind(parent).bind(running)
            .execute(&pool)
        };
        job(1, "still going",  "async", Some(1), Some(11)).await.unwrap();
        job(2, "just broke",   "async", Some(1), None).await.unwrap();
        job(3, "finished ok",  "async", Some(1), None).await.unwrap();
        job(4, "broke a while ago", "async", Some(1), None).await.unwrap();
        job(5, "someone else's", "async", Some(2), Some(55)).await.unwrap();
        job(6, "nightly digest", "cron", None,    Some(66)).await.unwrap();

        let run = |job_id: i64, session: i64, status: &'static str, completed: String| {
            sqlx::query(
                "INSERT INTO job_runs (job_id, session_id, started_at, completed_at,
                                       duration_ms, status, error)
                 VALUES (?, ?, '2026-08-04T10:00:00+00:00', ?, 10, ?, 'boom')",
            )
            .bind(job_id).bind(session).bind(completed).bind(status)
            .execute(&pool)
        };
        // RFC 3339, exactly as `run_job` writes it — the shape the window has to
        // cope with. A test that seeded SQLite-shaped strings here would pass
        // against a plain string comparison that production data defeats.
        let now = chrono::Utc::now();
        let at  = |m: i64| (now - chrono::Duration::minutes(m)).to_rfc3339();
        run(2, 22, "failed",    at(1)).await.unwrap();
        run(3, 33, "completed", at(1)).await.unwrap();
        run(4, 44, "failed",    at(120)).await.unwrap();

        pool
    }

    /// The strip shows what is running plus what has just broken — and nothing
    /// that belongs to another conversation, to the schedule, or to yesterday.
    #[tokio::test]
    async fn a_conversation_sees_its_running_and_recently_failed_tasks() {
        let pool  = seeded().await;
        let tasks = list_for_parent_session(&pool, 1, 30).await.unwrap();

        let seen: Vec<_> = tasks.iter().map(|t| (t.job_id, t.state.as_str())).collect();
        assert_eq!(seen, vec![(1, "running"), (2, "failed")]);

        // The drill-in target: the running job's live session, the failed one's run.
        assert_eq!(tasks[0].session_id, Some(11));
        assert_eq!(tasks[1].session_id, Some(22));
        assert_eq!(tasks[1].error.as_deref(), Some("boom"));
    }

    /// `running_since` is SQLite-shaped and `job_runs.started_at` is RFC 3339;
    /// both leave here as RFC 3339, or a browser reads one of them in the wrong
    /// timezone and shows an elapsed counter hours off.
    #[tokio::test]
    async fn started_at_is_normalised_to_rfc3339() {
        let pool  = seeded().await;
        let tasks = list_for_parent_session(&pool, 1, 30).await.unwrap();
        for task in &tasks {
            let raw = task.started_at.as_deref().expect("a started task has a start time");
            chrono::DateTime::parse_from_rfc3339(raw)
                .unwrap_or_else(|e| panic!("job {} start time {raw:?}: {e}", task.job_id));
        }
    }
}
