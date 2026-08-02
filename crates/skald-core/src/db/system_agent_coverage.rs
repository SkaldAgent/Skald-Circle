//! Accessor for `system_agent_coverage` — how far a background agent has
//! processed a given subject.
//!
//! This is the watermark that turns "everything since last time" into a
//! well-defined window `[covered_through, now)`. Two properties carry the whole
//! design, and both are the opposite of [`super::system_agent_state`]:
//!
//! - **It advances only on a completed pass.** A crash halfway leaves the mark
//!   where it was, so the next pass re-covers that stretch. For a review, a
//!   duplicate is a nuisance and a gap is a blind spot.
//! - **It is written after the work, not before.** `mark_attempt` is deliberately
//!   the first thing `run_and_record` does, which is exactly why it can never
//!   delimit the window the work is about.
//!
//! Timestamps are UTC `'YYYY-MM-DD HH:MM:SS'` — the format SQLite's
//! `datetime('now')` produces — so they compare as strings against the
//! `created_at` columns they are used to filter.

use anyhow::Result;
use sqlx::SqlitePool;

/// Format an instant the way SQLite's `datetime('now')` does, so the two are
/// string-comparable. The one place that knows the format.
pub fn stamp(at: chrono::DateTime<chrono::Utc>) -> String {
    at.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Now, in that format.
pub fn now_stamp() -> String {
    stamp(chrono::Utc::now())
}

/// How far `agent_id` has processed `subject`, or `None` if it never has.
pub async fn covered_through(
    pool:     &SqlitePool,
    agent_id: &str,
    subject:  &str,
) -> Result<Option<String>> {
    let at = sqlx::query_scalar::<_, String>(
        "SELECT covered_through FROM system_agent_coverage
         WHERE agent_id = ? AND subject_user_id = ?",
    )
    .bind(agent_id)
    .bind(subject)
    .fetch_optional(pool)
    .await?;
    Ok(at)
}

/// Move the watermark forward to `through`.
///
/// **Monotonic**: an older value than the one stored is ignored rather than
/// applied. Two passes for the same subject cannot run concurrently today (the
/// scheduler is sequential and single-instance), so this is not a race guard — it
/// is a guard against a caller computing a window start and writing *that* back
/// instead of the window end, which would silently make the agent re-read the
/// same stretch forever.
pub async fn advance(
    pool:     &SqlitePool,
    agent_id: &str,
    subject:  &str,
    through:  &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO system_agent_coverage (agent_id, subject_user_id, covered_through)
         VALUES (?, ?, ?)
         ON CONFLICT(agent_id, subject_user_id) DO UPDATE SET
             covered_through = MAX(system_agent_coverage.covered_through, excluded.covered_through),
             updated_at      = datetime('now')",
    )
    .bind(agent_id)
    .bind(subject)
    .bind(through)
    .execute(pool)
    .await?;
    Ok(())
}

/// Forget a subject's watermark, so the next pass starts from scratch. For an
/// admin-side "review this person again from the beginning".
pub async fn clear(pool: &SqlitePool, agent_id: &str, subject: &str) -> Result<bool> {
    let n = sqlx::query(
        "DELETE FROM system_agent_coverage WHERE agent_id = ? AND subject_user_id = ?",
    )
    .bind(agent_id)
    .bind(subject)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn registry() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::create_registry_tables(&pool).await.unwrap();
        sqlx::query("INSERT INTO roles (id, label, permission_group) VALUES ('member', 'Member', 'default')")
            .execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, role_id, encrypted)
             VALUES ('u-kid', 'kid', 'member', 0)",
        )
        .execute(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn never_covered_reads_as_none_then_advances() {
        let pool = registry().await;
        assert!(covered_through(&pool, "conversation-review", "u-kid").await.unwrap().is_none());

        advance(&pool, "conversation-review", "u-kid", "2026-07-28 04:00:00").await.unwrap();
        assert_eq!(
            covered_through(&pool, "conversation-review", "u-kid").await.unwrap().unwrap(),
            "2026-07-28 04:00:00",
        );

        advance(&pool, "conversation-review", "u-kid", "2026-07-29 04:00:00").await.unwrap();
        assert_eq!(
            covered_through(&pool, "conversation-review", "u-kid").await.unwrap().unwrap(),
            "2026-07-29 04:00:00",
        );
    }

    /// The guard that stops a caller from writing the window *start* back.
    #[tokio::test]
    async fn the_watermark_never_moves_backwards() {
        let pool = registry().await;
        advance(&pool, "conversation-review", "u-kid", "2026-07-29 04:00:00").await.unwrap();
        advance(&pool, "conversation-review", "u-kid", "2026-07-01 04:00:00").await.unwrap();
        assert_eq!(
            covered_through(&pool, "conversation-review", "u-kid").await.unwrap().unwrap(),
            "2026-07-29 04:00:00",
            "an older value must not rewind the watermark",
        );
    }

    #[tokio::test]
    async fn agents_and_subjects_do_not_share_a_row() {
        let pool = registry().await;
        sqlx::query(
            "INSERT INTO users (id, username, role_id, encrypted)
             VALUES ('u-two', 'two', 'member', 0)",
        )
        .execute(&pool).await.unwrap();

        advance(&pool, "conversation-review", "u-kid", "2026-07-29 04:00:00").await.unwrap();
        assert!(covered_through(&pool, "conversation-review", "u-two").await.unwrap().is_none());
        assert!(covered_through(&pool, "weekly-digest", "u-kid").await.unwrap().is_none());

        assert!(clear(&pool, "conversation-review", "u-kid").await.unwrap());
        assert!(!clear(&pool, "conversation-review", "u-kid").await.unwrap());
        assert!(covered_through(&pool, "conversation-review", "u-kid").await.unwrap().is_none());
    }

    #[test]
    fn the_stamp_matches_sqlite_datetime_shape() {
        let s = now_stamp();
        assert_eq!(s.len(), 19, "'YYYY-MM-DD HH:MM:SS'");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], " ");
        assert_eq!(&s[13..14], ":");
    }
}
