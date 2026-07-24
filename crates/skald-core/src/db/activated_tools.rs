//! Persisted tool-group activations (the effect of `activate_tools`).
//!
//! One row per activated group, anchored at the assistant `message_id` that
//! triggered it. Replaces the old `session_mcp_grants` / `stack_mcp_grants`
//! pair: `stack_id IS NULL` is a session-scoped activation (root agent), a
//! non-NULL `stack_id` is a sub-agent-frame activation (deleted on frame exit).
//!
//! The activation is the durable **effect**; the tool *call* itself lives in
//! `chat_llm_tools`. Keeping them separate means "which groups are active" is a
//! direct query, not a parse of `activate_tools` call arguments.
//!
//! `kind`/`ref` normalise the activated group: `('builtin', 'config')` for the
//! reserved built-in group, `('mcp', <server name>)` for an MCP server.

use anyhow::Result;
use sqlx::SqlitePool;

/// One activation row, anchored at the message that triggered it.
#[derive(Debug, Clone)]
pub struct Activation {
    /// The assistant `chat_history.id` whose tool call triggered this activation.
    pub message_id: i64,
    /// `'builtin'` (the reserved `config` group) or `'mcp'` (a server).
    pub kind: String,
    /// The group reference: `'config'`, or the MCP server name.
    pub ref_: String,
}

/// Persist a tool-group activation. `stack_id = None` → session-scoped (root
/// agent); `Some(id)` → stack-scoped (sub-agent frame). Idempotent via the
/// `COALESCE(stack_id, -1)`-based unique index (INSERT OR IGNORE).
pub async fn grant(
    pool: &SqlitePool,
    session_id: i64,
    stack_id: Option<i64>,
    message_id: i64,
    kind: &str,
    ref_: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO activated_tools (session_id, stack_id, message_id, kind, ref)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(session_id)
    .bind(stack_id)
    .bind(message_id)
    .bind(kind)
    .bind(ref_)
    .execute(pool)
    .await?;
    Ok(())
}

/// Session-scoped activated group refs (root agent). The in-memory grant set is
/// seeded from this at config-build time. Replaces
/// `session_mcp_grants::list_for_session`.
pub async fn list_refs_session(pool: &SqlitePool, session_id: i64) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT ref FROM activated_tools
         WHERE session_id = ? AND stack_id IS NULL
         ORDER BY id",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(r,)| r).collect())
}

/// Stack-scoped activated group refs (sub-agent frame). Sub-agents do **not**
/// inherit session-scoped grants — they start from their own frame only, exactly
/// as with the old `stack_mcp_grants::list_for_stack`.
pub async fn list_refs_stack(pool: &SqlitePool, stack_id: i64) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT ref FROM activated_tools
         WHERE stack_id = ?
         ORDER BY id",
    )
    .bind(stack_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(r,)| r).collect())
}

/// Activations in effect for one scope, up to and including `upto_message_id`,
/// ordered by the anchoring message. Used by the DTL serializer to place the
/// injected tool block (Kimi) at the position where it was activated. The scope
/// mirrors the in-memory grant set: root (`stack_id = None`) sees session-scoped
/// activations; a sub-agent (`Some(id)`) sees only its own frame's.
pub async fn list_active_at(
    pool: &SqlitePool,
    session_id: i64,
    stack_id: Option<i64>,
    upto_message_id: i64,
) -> Result<Vec<Activation>> {
    let rows = match stack_id {
        None => {
            sqlx::query_as::<_, (i64, String, String)>(
                "SELECT message_id, kind, ref FROM activated_tools
                 WHERE session_id = ? AND stack_id IS NULL AND message_id <= ?
                 ORDER BY message_id, id",
            )
            .bind(session_id)
            .bind(upto_message_id)
            .fetch_all(pool)
            .await?
        }
        Some(sid) => {
            sqlx::query_as::<_, (i64, String, String)>(
                "SELECT message_id, kind, ref FROM activated_tools
                 WHERE stack_id = ? AND message_id <= ?
                 ORDER BY message_id, id",
            )
            .bind(sid)
            .bind(upto_message_id)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows
        .into_iter()
        .map(|(message_id, kind, ref_)| Activation { message_id, kind, ref_ })
        .collect())
}

/// Clear all session-scoped activations for a session (the `/resettools` path).
/// Stack-scoped rows are ephemeral (removed on frame exit) and there are none
/// between turns, so only the session scope needs clearing.
pub async fn revoke_all_session(pool: &SqlitePool, session_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM activated_tools WHERE session_id = ? AND stack_id IS NULL")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Remove a stack frame's activations. Called when the frame terminates.
pub async fn delete_for_stack(pool: &SqlitePool, stack_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM activated_tools WHERE stack_id = ?")
        .bind(stack_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Re-anchor activations pinned to a message of `stack_id` that was just compacted
/// (chat_history id ≤ `covers_up_to`) onto `new_anchor` (the first surviving
/// message), so the DTL serializer still renders them after compaction instead of
/// losing the injection point. Scoped to this stack's messages via a subquery —
/// `message_id` is a global autoincrement, so a bare `<=` would also match other
/// stacks' rows. No unique-index conflict: only `message_id` changes, and there is
/// at most one row per `(session, stack, kind, ref)`.
pub async fn reanchor_compacted(
    pool: &SqlitePool,
    stack_id: i64,
    covers_up_to: i64,
    new_anchor: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE activated_tools SET message_id = ?
         WHERE message_id IN (
             SELECT id FROM chat_history
             WHERE session_stack_id = ? AND id <= ?
         )",
    )
    .bind(new_anchor)
    .bind(stack_id)
    .bind(covers_up_to)
    .execute(pool)
    .await?;
    Ok(())
}
