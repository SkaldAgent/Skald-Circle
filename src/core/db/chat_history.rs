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
    /// True for messages injected synthetically (e.g. TIC notifications) — not
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

pub async fn set_model_db_id(pool: &SqlitePool, id: i64, model_db_id: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE chat_history SET model_db_id = ? WHERE id = ?")
        .bind(model_db_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Ok messages for a stack frame whose id is strictly greater than `after_id`,
/// ordered chronologically.  Used by `build_openai_messages` when a compaction
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
