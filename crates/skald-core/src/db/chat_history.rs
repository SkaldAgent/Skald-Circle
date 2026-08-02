use sqlx::SqlitePool;

use core_api::message_meta::MessageMetadata;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    /// Invocation message from a calling agent to a sub-agent; mapped to `user`
    /// when rebuilding LLM context, invisible in the UI.
    Agent,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User      => "user",
            Role::Assistant => "assistant",
            Role::Agent     => "agent",
        }
    }

    pub fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "user"      => Ok(Role::User),
            "assistant" => Ok(Role::Assistant),
            "agent"     => Ok(Role::Agent),
            other       => anyhow::bail!("Unknown role: {other}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub id:                i64,
    pub role:              Role,
    pub content:           String,
    pub status:            String,
    pub input_tokens:      Option<i64>,
    pub output_tokens:     Option<i64>,
    /// True for messages injected synthetically (e.g. event triage notifications) — not
    /// typed by a real user.  Stored in DB so the UI can skip them on reload.
    pub is_synthetic:      bool,
    /// Chain-of-thought from reasoning models (e.g. DeepSeek thinking mode).
    /// Null for all other providers.
    pub reasoning_content: Option<String>,
    /// Cost of the turn in USD, when the provider reports it (OpenRouter).
    /// Null for providers that don't bill per-request.
    pub cost:              Option<f64>,
    /// Generic structured metadata (JSON column): file attachments today,
    /// extensible later. `None` when the row has no metadata.
    pub metadata:          Option<MessageMetadata>,
    pub created_at:        Option<String>,
}

/// Raw row tuple for the shared `SELECT` projection. sqlx 0.9 requires SQL to be
/// `&'static str`, so the column list is repeated literally in each query below;
/// keep it in sync with this tuple and [`row_to_message`].
type Row = (
    i64, String, String, String, Option<i64>, Option<i64>, bool,
    Option<String>, Option<f64>, Option<String>, Option<String>,
);

/// Maps a [`Row`] into a [`ChatMessage`]. Metadata that fails to parse is treated
/// as absent (defensive: a malformed blob must not break history loading).
fn row_to_message(r: Row) -> anyhow::Result<ChatMessage> {
    let (id, role, content, status, input_tokens, output_tokens, is_synthetic, reasoning_content, cost, metadata, created_at) = r;
    Ok(ChatMessage {
        id,
        role: Role::from_str(&role)?,
        content,
        status,
        input_tokens,
        output_tokens,
        is_synthetic,
        reasoning_content,
        cost,
        metadata: metadata.and_then(|s| serde_json::from_str(&s).ok()),
        created_at,
    })
}

/// Appends a message with no structured metadata (the common case).
pub async fn append(
    pool:              &SqlitePool,
    session_stack_id:  i64,
    role:              &Role,
    content:           &str,
    is_synthetic:      bool,
    reasoning_content: Option<&str>,
) -> anyhow::Result<i64> {
    append_with_metadata(pool, session_stack_id, role, content, is_synthetic, reasoning_content, None).await
}

/// Like [`append`] but persists optional structured [`MessageMetadata`] (e.g. file
/// attachments) as a JSON blob. Empty metadata is stored as `NULL`.
pub async fn append_with_metadata(
    pool:              &SqlitePool,
    session_stack_id:  i64,
    role:              &Role,
    content:           &str,
    is_synthetic:      bool,
    reasoning_content: Option<&str>,
    metadata:          Option<&MessageMetadata>,
) -> anyhow::Result<i64> {
    let metadata_json = metadata
        .filter(|m| !m.is_empty())
        .map(serde_json::to_string)
        .transpose()?;
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO chat_history (session_stack_id, role, content, is_synthetic, reasoning_content, metadata) \
         VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(session_stack_id)
    .bind(role.as_str())
    .bind(content)
    .bind(is_synthetic as i64)
    .bind(reasoning_content)
    .bind(metadata_json)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

pub async fn mark_failed(pool: &SqlitePool, id: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE chat_history SET status = 'failed' WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_usage(
    pool:          &SqlitePool,
    id:            i64,
    input_tokens:  u32,
    output_tokens: u32,
    duration_ms:   u64,
    cost:          Option<f64>,
) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE chat_history
         SET input_tokens = ?, output_tokens = ?, duration_ms = ?, cost = ?
         WHERE id = ?",
    )
    .bind(input_tokens as i64)
    .bind(output_tokens as i64)
    .bind(duration_ms as i64)
    .bind(cost)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// All ok messages for a stack frame, ordered chronologically.
/// Used to rebuild LLM context for a specific agent.
pub async fn for_stack(
    pool:             &SqlitePool,
    session_stack_id: i64,
) -> anyhow::Result<Vec<ChatMessage>> {
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, role, content, status, input_tokens, output_tokens, is_synthetic, reasoning_content, cost, metadata, created_at
         FROM   chat_history
         WHERE  session_stack_id = ? AND status = 'ok'
         ORDER  BY id ASC",
    )
    .bind(session_stack_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(row_to_message).collect()
}

/// All messages for a stack frame including failed ones, ordered chronologically.
/// Used by the UI history API so the user can see cancelled messages.
pub async fn for_stack_all(
    pool:             &SqlitePool,
    session_stack_id: i64,
) -> anyhow::Result<Vec<ChatMessage>> {
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, role, content, status, input_tokens, output_tokens, is_synthetic, reasoning_content, cost, metadata, created_at
         FROM   chat_history
         WHERE  session_stack_id = ?
         ORDER  BY id ASC",
    )
    .bind(session_stack_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(row_to_message).collect()
}

/// Ok messages for a stack frame whose id is strictly greater than `after_id`,
/// ordered chronologically.  Used by the projection when a compaction
/// summary exists: only the "raw" messages after the summary boundary are loaded.
pub async fn for_stack_since(
    pool:             &SqlitePool,
    session_stack_id: i64,
    after_id:         i64,
) -> anyhow::Result<Vec<ChatMessage>> {
    let rows = sqlx::query_as::<_, Row>(
        "SELECT id, role, content, status, input_tokens, output_tokens, is_synthetic, reasoning_content, cost, metadata, created_at
         FROM   chat_history
         WHERE  session_stack_id = ? AND status = 'ok' AND id > ?
         ORDER  BY id ASC",
    )
    .bind(session_stack_id)
    .bind(after_id)
    .fetch_all(pool)
    .await?;

    rows.into_iter().map(row_to_message).collect()
}

/// One line of a cross-session transcript: a message with the conversation it
/// belongs to. See [`conversation_window`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TranscriptLine {
    pub session_id:    i64,
    pub session_title: Option<String>,
    pub source:        String,
    pub agent_id:      String,
    pub role:          String,
    pub content:       String,
    pub created_at:    String,
}

/// Every message this database's owner exchanged with an assistant between
/// `since` (inclusive) and `until` (exclusive), across **all** their
/// conversations, oldest first.
///
/// The window is half-open so consecutive calls tile without overlapping or
/// skipping: today's `until` is tomorrow's `since`. Both bounds are UTC
/// `'YYYY-MM-DD HH:MM:SS'`, the shape `datetime('now')` writes, so they compare
/// as plain strings against `created_at`.
///
/// **Four filters, and each one exists because of a specific way the result would
/// otherwise be wrong:**
///
/// - `is_ephemeral = 0` — a background agent's own throwaway sessions live in the
///   same table. Without this, a pass that reads conversations would read the
///   transcript its *previous* pass was given, and report on itself.
/// - `depth = 0` — only the root frame. Deeper frames are sub-agents talking to
///   each other: machine-to-machine chatter that nobody typed.
/// - `is_synthetic = 0` — turns the machinery injected as if they were the user
///   (notification briefings, job results). Attributing those to the person would
///   be a lie about who said what.
/// - `content <> ''` — an assistant row whose whole content was a tool call.
///
/// **Tool calls and their results are not here at all**, and that is by
/// construction rather than by filter: they live in `chat_llm_tools`, keyed to a
/// message id. So this returns what was *said*, never what was *done* — a web
/// search the assistant ran is invisible, including its query.
pub async fn conversation_window(
    pool:  &SqlitePool,
    since: &str,
    until: &str,
    limit: i64,
) -> anyhow::Result<Vec<TranscriptLine>> {
    // Newest-first with a LIMIT, then reversed: over budget, the window that
    // matters is the recent end, not whatever happened to come first.
    let mut rows = sqlx::query_as::<_, TranscriptLine>(
        "SELECT s.id           AS session_id,
                s.title        AS session_title,
                s.source       AS source,
                s.agent_id     AS agent_id,
                h.role         AS role,
                h.content      AS content,
                h.created_at   AS created_at
         FROM chat_history h
         JOIN chat_sessions_stack st ON st.id = h.session_stack_id
         JOIN chat_sessions       s  ON s.id  = st.session_id
         WHERE h.created_at >= ? AND h.created_at < ?
           AND h.status       = 'ok'
           AND h.is_synthetic = 0
           AND h.content     <> ''
           AND h.role IN ('user', 'assistant')
           AND st.depth       = 0
           AND s.is_ephemeral = 0
         ORDER BY h.created_at DESC, h.id DESC
         LIMIT ?",
    )
    .bind(since)
    .bind(until)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    rows.reverse();
    Ok(rows)
}

/// How many messages [`conversation_window`] would return, without loading them.
/// The cheap look a scheduler takes before deciding a pass is worth opening.
pub async fn conversation_window_count(
    pool:  &SqlitePool,
    since: &str,
    until: &str,
) -> anyhow::Result<i64> {
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT count(*)
         FROM chat_history h
         JOIN chat_sessions_stack st ON st.id = h.session_stack_id
         JOIN chat_sessions       s  ON s.id  = st.session_id
         WHERE h.created_at >= ? AND h.created_at < ?
           AND h.status       = 'ok'
           AND h.is_synthetic = 0
           AND h.content     <> ''
           AND h.role IN ('user', 'assistant')
           AND st.depth       = 0
           AND s.is_ephemeral = 0",
    )
    .bind(since)
    .bind(until)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

/// The last thing the assistant said in a session's **root** frame.
///
/// For a caller whose agent produces a document rather than a side effect: the
/// turn's answer is the deliverable, and it has to be read back from the store
/// because `handle_message` returns nothing. Root frame only — the deepest
/// sub-agent's last words are not the session's answer.
pub async fn last_assistant_for_session(
    pool:       &SqlitePool,
    session_id: i64,
) -> anyhow::Result<Option<String>> {
    let content = sqlx::query_scalar::<_, String>(
        "SELECT h.content
         FROM chat_history h
         JOIN chat_sessions_stack st ON st.id = h.session_stack_id
         WHERE st.session_id = ? AND st.depth = 0
           AND h.role = 'assistant' AND h.status = 'ok' AND h.content <> ''
         ORDER BY h.id DESC
         LIMIT 1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    Ok(content)
}

/// Returns the most recent ok message for a stack frame, or `None` if empty.
/// Used by Telegram's `/context` command to show last turn's token usage.
pub async fn last_message_for_stack(
    pool:             &SqlitePool,
    session_stack_id: i64,
) -> anyhow::Result<Option<ChatMessage>> {
    let row = sqlx::query_as::<_, Row>(
        "SELECT id, role, content, status, input_tokens, output_tokens, is_synthetic, reasoning_content, cost, metadata, created_at
         FROM   chat_history
         WHERE  session_stack_id = ? AND status = 'ok'
         ORDER  BY id DESC
         LIMIT  1",
    )
    .bind(session_stack_id)
    .fetch_optional(pool)
    .await?;

    row.map(row_to_message).transpose()
}

/// Total cost (USD) of a whole session: all messages across every stack frame
/// (main + sync sub-agents) that share this `session_id`. Async tasks live in
/// their own session and are naturally excluded. Returns `None` when no message
/// has a recorded cost (e.g. the provider does not report per-request pricing).
///
/// No `status` filter: money is spent even on turns later marked `failed`, so the
/// total reflects real spend. Uses plain `SUM(cost)` so an all-NULL set yields
/// `None`, distinguishing "no cost data" from a genuine `$0.00`.
pub async fn total_cost_for_session(
    pool:       &SqlitePool,
    session_id: i64,
) -> anyhow::Result<Option<f64>> {
    let total: Option<f64> = sqlx::query_scalar(
        "SELECT SUM(ch.cost)
         FROM   chat_history ch
         JOIN   chat_sessions_stack css ON ch.session_stack_id = css.id
         WHERE  css.session_id = ?",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await?;
    Ok(total)
}

/// Rough token estimate for a stack frame (sum of content lengths / 4).
/// Used as a fallback when the LLM provider does not return usage data.
pub async fn estimate_tokens_for_stack(
    pool:             &SqlitePool,
    session_stack_id: i64,
) -> anyhow::Result<u32> {
    let total_chars: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(LENGTH(content)), 0)
         FROM   chat_history
         WHERE  session_stack_id = ? AND status = 'ok'",
    )
    .bind(session_stack_id)
    .fetch_one(pool)
    .await?;

    Ok((total_chars / 4).max(0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A standalone owner-schema database with one ordinary conversation and one
    /// of every thing the window must leave out.
    async fn seeded() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::create_owner_tables(&pool).await.unwrap();

        let q = |sql: &'static str| sqlx::query(sql).execute(&pool);

        // A real conversation, and a second one the same day.
        q("INSERT INTO chat_sessions (id, title, source, agent_id, is_ephemeral) VALUES (1, 'Homework', 'web', 'kid', 0)").await.unwrap();
        q("INSERT INTO chat_sessions (id, title, source, agent_id, is_ephemeral) VALUES (2, NULL, 'telegram', 'kid', 0)").await.unwrap();
        // A background agent's throwaway session — the one that would make a
        // review read its own previous pass.
        q("INSERT INTO chat_sessions (id, title, source, agent_id, is_ephemeral) VALUES (3, 'review', 'conversation-review', 'conversation-review', 1)").await.unwrap();

        q("INSERT INTO chat_sessions_stack (id, session_id, depth) VALUES (1, 1, 0)").await.unwrap();
        q("INSERT INTO chat_sessions_stack (id, session_id, depth) VALUES (2, 2, 0)").await.unwrap();
        q("INSERT INTO chat_sessions_stack (id, session_id, depth) VALUES (3, 3, 0)").await.unwrap();
        // A sub-agent frame of the real conversation.
        q("INSERT INTO chat_sessions_stack (id, session_id, depth) VALUES (4, 1, 1)").await.unwrap();

        let msg = |stack: i64, role: &'static str, content: &'static str, at: &'static str,
                   synthetic: i64, status: &'static str| {
            sqlx::query(
                "INSERT INTO chat_history (session_stack_id, role, content, created_at, is_synthetic, status)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(stack).bind(role).bind(content).bind(at).bind(synthetic).bind(status)
            .execute(&pool)
        };

        msg(1, "user",      "kept: in window",      "2026-07-28 21:00:00", 0, "ok").await.unwrap();
        msg(1, "assistant", "kept: the reply",      "2026-07-28 21:00:30", 0, "ok").await.unwrap();
        msg(2, "user",      "kept: other session",  "2026-07-29 02:00:00", 0, "ok").await.unwrap();

        msg(1, "user",      "dropped: before",      "2026-07-27 10:00:00", 0, "ok").await.unwrap();
        msg(1, "user",      "dropped: after",       "2026-07-30 10:00:00", 0, "ok").await.unwrap();
        msg(3, "user",      "dropped: ephemeral",   "2026-07-28 22:00:00", 0, "ok").await.unwrap();
        msg(4, "assistant", "dropped: sub-agent",   "2026-07-28 22:00:00", 0, "ok").await.unwrap();
        msg(1, "user",      "dropped: synthetic",   "2026-07-28 22:00:00", 1, "ok").await.unwrap();
        msg(1, "assistant", "dropped: failed",      "2026-07-28 22:00:00", 0, "failed").await.unwrap();
        msg(1, "assistant", "",                     "2026-07-28 22:00:00", 0, "ok").await.unwrap();
        msg(1, "agent",     "dropped: agent role",  "2026-07-28 22:00:00", 0, "ok").await.unwrap();

        pool
    }

    const SINCE: &str = "2026-07-28 04:00:00";
    const UNTIL: &str = "2026-07-29 04:00:00";

    /// Each exclusion is a way the review would otherwise be wrong; assert them
    /// together, because it is the *set* that defines "what was said".
    #[tokio::test]
    async fn the_window_keeps_only_what_was_said_in_it() {
        let pool = seeded().await;

        let lines = conversation_window(&pool, SINCE, UNTIL, 100).await.unwrap();
        let kept: Vec<&str> = lines.iter().map(|l| l.content.as_str()).collect();

        assert_eq!(kept, vec![
            "kept: in window",
            "kept: the reply",
            "kept: other session",
        ], "everything else is a way the transcript would lie");

        // The count is the same question, asked cheaply.
        assert_eq!(conversation_window_count(&pool, SINCE, UNTIL).await.unwrap(), 3);

        // Oldest first, and each line carries the conversation it belongs to.
        assert_eq!(lines[0].session_id, 1);
        assert_eq!(lines[0].session_title.as_deref(), Some("Homework"));
        assert_eq!(lines[2].session_id, 2);
        assert_eq!(lines[2].source, "telegram");
        assert!(lines[2].session_title.is_none());
    }

    /// Half-open, so consecutive windows tile: a message exactly on the boundary
    /// belongs to the later window, never to both and never to neither.
    #[tokio::test]
    async fn the_window_is_half_open() {
        let pool = seeded().await;
        sqlx::query(
            "INSERT INTO chat_history (session_stack_id, role, content, created_at)
             VALUES (1, 'user', 'exactly on the edge', ?)",
        )
        .bind(UNTIL)
        .execute(&pool).await.unwrap();

        let before = conversation_window(&pool, SINCE, UNTIL, 100).await.unwrap();
        assert!(!before.iter().any(|l| l.content == "exactly on the edge"),
                "`until` is exclusive");

        let after = conversation_window(&pool, UNTIL, "2026-07-30 04:00:00", 100).await.unwrap();
        assert!(after.iter().any(|l| l.content == "exactly on the edge"),
                "`since` is inclusive, so nothing falls between two windows");
    }

    /// Over budget, the recent end is what survives — a truncated review of last
    /// night beats a complete review of last month.
    #[tokio::test]
    async fn a_capped_window_keeps_the_most_recent_messages() {
        let pool = seeded().await;

        let lines = conversation_window(&pool, SINCE, UNTIL, 2).await.unwrap();
        let kept: Vec<&str> = lines.iter().map(|l| l.content.as_str()).collect();
        assert_eq!(kept, vec!["kept: the reply", "kept: other session"]);

        // The count ignores the cap, which is how the caller knows it truncated.
        assert_eq!(conversation_window_count(&pool, SINCE, UNTIL).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn the_last_assistant_message_comes_from_the_root_frame() {
        let pool = seeded().await;

        // Session 1's newest root-frame assistant line, not the sub-agent's.
        sqlx::query(
            "INSERT INTO chat_history (session_stack_id, role, content, created_at)
             VALUES (1, 'assistant', 'the answer', '2026-07-29 03:00:00')",
        )
        .execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO chat_history (session_stack_id, role, content, created_at)
             VALUES (4, 'assistant', 'sub-agent chatter', '2026-07-29 03:30:00')",
        )
        .execute(&pool).await.unwrap();

        assert_eq!(
            last_assistant_for_session(&pool, 1).await.unwrap().as_deref(),
            Some("the answer"),
        );
        // A session that never got an answer says so rather than inventing one.
        assert!(last_assistant_for_session(&pool, 2).await.unwrap().is_none());
        assert!(last_assistant_for_session(&pool, 99).await.unwrap().is_none());
    }
}
