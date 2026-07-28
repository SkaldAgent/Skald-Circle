//! Scheduler state for the system agents: when each one last *attempted* a pass
//! for this user.
//!
//! Deliberately separate from [`super::system_agent_runs`], which is a history
//! written for the human and skips idle ticks. Due-ness needs every attempt, so
//! it needs its own row — see the table comment in [`super::create_owner_tables`]
//! for why conflating the two breaks both.
//!
//! Owner table, no `user_id` column: the file is the owner (§5.1).

use anyhow::Result;
use sqlx::SqlitePool;

/// When `agent_id` last attempted a pass here, as a SQLite `datetime('now')`
/// string, or `None` if it never has.
pub async fn last_attempt_at(pool: &SqlitePool, agent_id: &str) -> Result<Option<String>> {
    let at = sqlx::query_scalar::<_, String>(
        "SELECT last_attempt_at FROM system_agent_state WHERE agent_id = ?",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await?;
    Ok(at)
}

/// Record an attempt as of now. Called whether or not the pass had anything to
/// do — that is the whole point of this table.
pub async fn mark_attempt(pool: &SqlitePool, agent_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO system_agent_state (agent_id, last_attempt_at)
         VALUES (?, datetime('now'))
         ON CONFLICT(agent_id) DO UPDATE SET last_attempt_at = datetime('now')",
    )
    .bind(agent_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Seconds since the last attempt, or `None` when there has never been one
/// (which every caller must read as "due now").
pub async fn seconds_since_attempt(pool: &SqlitePool, agent_id: &str) -> Result<Option<i64>> {
    let secs = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT CAST(strftime('%s', 'now') AS INTEGER)
              - CAST(strftime('%s', last_attempt_at) AS INTEGER)
         FROM system_agent_state WHERE agent_id = ?",
    )
    .bind(agent_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::create_owner_tables(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn never_attempted_reads_as_due() {
        let pool = pool().await;
        assert!(last_attempt_at(&pool, "event-triage").await.unwrap().is_none());
        assert!(seconds_since_attempt(&pool, "event-triage").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_attempt_is_recorded_and_then_overwritten() {
        let pool = pool().await;
        mark_attempt(&pool, "event-triage").await.unwrap();
        let first = last_attempt_at(&pool, "event-triage").await.unwrap().unwrap();

        // Fresh attempt: still one row for this agent, and the age is small.
        mark_attempt(&pool, "event-triage").await.unwrap();
        assert!(seconds_since_attempt(&pool, "event-triage").await.unwrap().unwrap() < 5);
        assert!(!first.is_empty());

        let rows = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM system_agent_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "mark_attempt must upsert, not accumulate");
    }

    #[tokio::test]
    async fn agents_do_not_share_a_row() {
        let pool = pool().await;
        mark_attempt(&pool, "event-triage").await.unwrap();
        assert!(seconds_since_attempt(&pool, "event-triage").await.unwrap().is_some());
        assert!(seconds_since_attempt(&pool, "memory-lint").await.unwrap().is_none());
    }
}
