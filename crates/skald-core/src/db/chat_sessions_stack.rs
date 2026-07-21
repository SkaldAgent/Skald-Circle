use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct SessionStack {
    pub id:                  i64,
    pub agent_id:            String,
    pub depth:               i64,
    pub parent_tool_call_id: Option<i64>,
}

pub async fn create(
    pool:                &SqlitePool,
    session_id:          i64,
    agent_id:            &str,
    agent_prompt:        Option<&str>,
    depth:               i64,
    parent_tool_call_id: Option<i64>,
) -> anyhow::Result<SessionStack> {
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO chat_sessions_stack (session_id, agent_id, agent_prompt, depth, parent_tool_call_id)
         VALUES (?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(session_id)
    .bind(agent_id)
    .bind(agent_prompt)
    .bind(depth)
    .bind(parent_tool_call_id)
    .fetch_one(pool)
    .await?;

    Ok(SessionStack { id, agent_id: agent_id.to_string(), depth, parent_tool_call_id })
}

/// Returns the deepest active (non-terminated) frame for a session.
pub async fn active_for_session(
    pool:       &SqlitePool,
    session_id: i64,
) -> anyhow::Result<Option<SessionStack>> {
    let row = sqlx::query_as::<_, (i64, String, i64, Option<i64>)>(
        "SELECT id, agent_id, depth, parent_tool_call_id
         FROM   chat_sessions_stack
         WHERE  session_id    = ?
           AND  terminated_at IS NULL
         ORDER  BY depth DESC
         LIMIT  1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(row_to_stack))
}

/// All active (non-terminated) frames for a session, ordered by depth ASC.
/// Used by restart recovery to detect an interrupted parallel sub-agent batch:
/// a purely linear stack has at most one active frame per depth, so ≥2 active
/// frames at the same depth can only be a concurrent batch left mid-flight.
pub async fn active_all_for_session(
    pool:       &SqlitePool,
    session_id: i64,
) -> anyhow::Result<Vec<SessionStack>> {
    let rows = sqlx::query_as::<_, (i64, String, i64, Option<i64>)>(
        "SELECT id, agent_id, depth, parent_tool_call_id
         FROM   chat_sessions_stack
         WHERE  session_id    = ?
           AND  terminated_at IS NULL
         ORDER  BY depth ASC",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(row_to_stack).collect())
}

/// Returns the root (depth=0) stack frame for a session.
pub async fn main_for_session(
    pool:       &SqlitePool,
    session_id: i64,
) -> anyhow::Result<Option<SessionStack>> {
    let row = sqlx::query_as::<_, (i64, String, i64, Option<i64>)>(
        "SELECT id, agent_id, depth, parent_tool_call_id
         FROM   chat_sessions_stack
         WHERE  session_id = ? AND depth = 0
         ORDER  BY id ASC
         LIMIT  1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_stack))
}

/// Returns all stack frames for a session (including terminated), ordered by id ASC.
/// Used to reconstruct the full agent call tree from history.
pub async fn all_for_session(
    pool:       &SqlitePool,
    session_id: i64,
) -> anyhow::Result<Vec<SessionStack>> {
    let rows = sqlx::query_as::<_, (i64, String, i64, Option<i64>)>(
        "SELECT id, agent_id, depth, parent_tool_call_id
         FROM   chat_sessions_stack
         WHERE  session_id = ?
         ORDER  BY id ASC",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(row_to_stack).collect())
}

pub async fn find_by_id(pool: &SqlitePool, id: i64) -> anyhow::Result<Option<SessionStack>> {
    let row = sqlx::query_as::<_, (i64, String, i64, Option<i64>)>(
        "SELECT id, agent_id, depth, parent_tool_call_id
         FROM   chat_sessions_stack
         WHERE  id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(row_to_stack))
}

/// Marks a stack frame as terminated (agent completed or was cancelled).
pub async fn terminate(pool: &SqlitePool, id: i64) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE chat_sessions_stack SET terminated_at = datetime('now') WHERE id = ?",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

fn row_to_stack(
    (id, agent_id, depth, parent_tool_call_id): (i64, String, i64, Option<i64>),
) -> SessionStack {
    SessionStack { id, agent_id, depth, parent_tool_call_id }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        p.push(format!("skald-test-{tag}-{}-{nanos}.db", std::process::id()));
        p.to_string_lossy().into_owned()
    }

    fn cleanup(path: &str) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
    }

    /// Validates `active_all_for_session` against the real schema: two active
    /// frames at the same depth are the signature of an interrupted parallel
    /// sub-agent batch, and terminating one drops it from the active set.
    #[tokio::test]
    async fn active_all_reflects_parallel_siblings() {
        let path = temp_db_path("stack-parallel");
        let pool = crate::db::init_system_pool(&path).await.unwrap();
        let sid  = 1;
        // `session_id` has a FK to chat_sessions (sqlx enables foreign_keys).
        sqlx::query("INSERT INTO chat_sessions (id) VALUES (?)")
            .bind(sid).execute(&pool).await.unwrap();

        create(&pool, sid, "assistant", None,      0, None).await.unwrap();
        let a = create(&pool, sid, "task", Some("A"), 1, Some(101)).await.unwrap();
        create(&pool, sid, "task", Some("B"), 1, Some(102)).await.unwrap();

        let active = active_all_for_session(&pool, sid).await.unwrap();
        assert_eq!(active.len(), 3, "root + two live siblings");
        assert_eq!(active.iter().filter(|f| f.depth == 1).count(), 2, "two frames share depth 1");

        // Terminating one sibling removes it from the active set (used by reap).
        terminate(&pool, a.id).await.unwrap();
        let active = active_all_for_session(&pool, sid).await.unwrap();
        assert_eq!(active.len(), 2);
        assert!(active.iter().all(|f| f.id != a.id), "terminated frame is excluded");

        pool.close().await;
        cleanup(&path);
    }
}
