use anyhow::Result;
use sqlx::SqlitePool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobRun {
    pub id:             i64,
    pub job_id:         i64,
    pub session_id:     Option<i64>,
    pub started_at:     String,
    pub completed_at:   Option<String>,
    pub duration_ms:    Option<i64>,
    pub status:         String,
    pub final_response: Option<String>,
    pub error:          Option<String>,
    pub created_at:     String,
}

pub async fn insert(
    pool:           &SqlitePool,
    job_id:         i64,
    session_id:     Option<i64>,
    started_at:     &str,
    completed_at:   &str,
    duration_ms:    i64,
    status:         &str,
    final_response: Option<&str>,
    error:          Option<&str>,
) -> Result<JobRun> {
    let id = sqlx::query(
        "INSERT INTO job_runs (job_id, session_id, started_at, completed_at, duration_ms, status, final_response, error)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(job_id)
    .bind(session_id)
    .bind(started_at)
    .bind(completed_at)
    .bind(duration_ms)
    .bind(status)
    .bind(final_response)
    .bind(error)
    .execute(pool)
    .await?
    .last_insert_rowid();

    let row = sqlx::query_as::<_, JobRun>(
        "SELECT id, job_id, session_id, started_at, completed_at, duration_ms,
                status, final_response, error, created_at
         FROM job_runs WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn list_for_job(pool: &SqlitePool, job_id: i64, limit: i64) -> Result<Vec<JobRun>> {
    let rows = sqlx::query_as::<_, JobRun>(
        "SELECT id, job_id, session_id, started_at, completed_at, duration_ms,
                status, final_response, error, created_at
         FROM job_runs
         WHERE job_id = ?
         ORDER BY created_at DESC
         LIMIT ?",
    )
    .bind(job_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct JobRunWithMeta {
    pub id:             i64,
    pub job_id:         i64,
    pub session_id:     Option<i64>,
    pub started_at:     String,
    pub completed_at:   Option<String>,
    pub duration_ms:    Option<i64>,
    pub status:         String,
    pub final_response: Option<String>,
    pub error:          Option<String>,
    pub created_at:     String,
    pub job_title:      Option<String>,
    pub agent_id:       Option<String>,
    pub kind:           Option<String>,
}

pub async fn list_all(pool: &SqlitePool, limit: i64) -> Result<Vec<JobRunWithMeta>> {
    let rows = sqlx::query_as::<_, JobRunWithMeta>(
        "SELECT jr.id, jr.job_id, jr.session_id, jr.started_at, jr.completed_at,
                jr.duration_ms, jr.status, jr.final_response, jr.error, jr.created_at,
                sj.title AS job_title, sj.agent_id, sj.kind
         FROM job_runs jr
         LEFT JOIN scheduled_jobs sj ON jr.job_id = sj.id
         ORDER BY jr.created_at DESC
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
