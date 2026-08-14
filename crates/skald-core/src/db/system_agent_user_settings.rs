//! Accessor for `system_agent_user_settings` — per-user overrides of a system
//! agent's schedule.
//!
//! The whole contract is in the absence of a row: **no row means the instance
//! setting applies**, so every read here answers `Option` and every caller falls
//! back rather than defaulting. Clearing an override therefore [`clear`]s the row
//! instead of writing a sentinel — a `0` or a `-1` standing for "inherit" would
//! be a second way to say what the empty table already says, and the two would
//! eventually disagree.
//!
//! Registry table: written by an admin about a member, from the Users page. See
//! the table comment in [`super::create_registry_tables`] for why it cannot live
//! in the member's own file.

use anyhow::Result;
use sqlx::SqlitePool;

/// One user's override of `agent_id`'s interval, in seconds, or `None` when they
/// have none and the instance-wide setting stands.
pub async fn interval_secs(
    pool:     &SqlitePool,
    agent_id: &str,
    user_id:  &str,
) -> Result<Option<i64>> {
    let secs = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT interval_secs FROM system_agent_user_settings
         WHERE agent_id = ? AND user_id = ?",
    )
    .bind(agent_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    Ok(secs)
}

/// Set `user_id`'s override for `agent_id`.
pub async fn set_interval_secs(
    pool:     &SqlitePool,
    agent_id: &str,
    user_id:  &str,
    secs:     i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO system_agent_user_settings (agent_id, user_id, interval_secs)
         VALUES (?, ?, ?)
         ON CONFLICT(agent_id, user_id) DO UPDATE SET
             interval_secs = excluded.interval_secs,
             updated_at    = datetime('now')",
    )
    .bind(agent_id)
    .bind(user_id)
    .bind(secs)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop `user_id`'s override, so they follow the instance setting again.
pub async fn clear(pool: &SqlitePool, agent_id: &str, user_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM system_agent_user_settings WHERE agent_id = ? AND user_id = ?")
        .bind(agent_id)
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The shortest override anyone holds for `agent_id`, or `None` when nobody
/// overrides it.
///
/// Exists for the scheduler's wake-up: it sleeps for the shortest interval any
/// enabled agent asks for, and an override *below* the instance value would
/// otherwise be rounded up to it — silently, and only in that direction, which is
/// the kind of half-working setting that is worse than one that does nothing.
pub async fn shortest_interval_secs(pool: &SqlitePool, agent_id: &str) -> Result<Option<i64>> {
    let secs = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MIN(interval_secs) FROM system_agent_user_settings
         WHERE agent_id = ? AND interval_secs IS NOT NULL",
    )
    .bind(agent_id)
    .fetch_one(pool)
    .await?;
    Ok(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGENT: &str = "event-triage";

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::create_registry_tables(&pool).await.unwrap();
        crate::db::roles::seed_admin(&pool).await.unwrap();
        for id in ["alice", "bob"] {
            sqlx::query(
                "INSERT INTO users (id, username, role_id, encrypted) VALUES (?, ?, 'admin', 0)",
            )
            .bind(id)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool
    }

    #[tokio::test]
    async fn no_row_means_inherit() {
        let pool = pool().await;
        assert_eq!(interval_secs(&pool, AGENT, "alice").await.unwrap(), None);
        assert_eq!(shortest_interval_secs(&pool, AGENT).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_override_is_set_then_replaced_then_cleared() {
        let pool = pool().await;
        set_interval_secs(&pool, AGENT, "alice", 3600).await.unwrap();
        assert_eq!(interval_secs(&pool, AGENT, "alice").await.unwrap(), Some(3600));

        set_interval_secs(&pool, AGENT, "alice", 1800).await.unwrap();
        assert_eq!(interval_secs(&pool, AGENT, "alice").await.unwrap(), Some(1800));
        let rows = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM system_agent_user_settings")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1, "setting an override must upsert, not accumulate");

        clear(&pool, AGENT, "alice").await.unwrap();
        assert_eq!(interval_secs(&pool, AGENT, "alice").await.unwrap(), None);
    }

    #[tokio::test]
    async fn users_and_agents_do_not_share_a_row() {
        let pool = pool().await;
        set_interval_secs(&pool, AGENT, "alice", 3600).await.unwrap();
        assert_eq!(interval_secs(&pool, AGENT, "bob").await.unwrap(), None);
        assert_eq!(interval_secs(&pool, "memory-lint", "alice").await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_shortest_override_is_the_scheduler_floor() {
        let pool = pool().await;
        set_interval_secs(&pool, AGENT, "alice", 3600).await.unwrap();
        set_interval_secs(&pool, AGENT, "bob", 120).await.unwrap();
        assert_eq!(shortest_interval_secs(&pool, AGENT).await.unwrap(), Some(120));
        // Another agent's overrides must not drag this one's wake-up down.
        set_interval_secs(&pool, "memory-lint", "alice", 60).await.unwrap();
        assert_eq!(shortest_interval_secs(&pool, AGENT).await.unwrap(), Some(120));
    }

    #[tokio::test]
    async fn deleting_a_user_takes_their_overrides() {
        let pool = pool().await;
        set_interval_secs(&pool, AGENT, "alice", 3600).await.unwrap();
        sqlx::query("DELETE FROM users WHERE id = 'alice'").execute(&pool).await.unwrap();
        assert_eq!(interval_secs(&pool, AGENT, "alice").await.unwrap(), None);
    }
}
