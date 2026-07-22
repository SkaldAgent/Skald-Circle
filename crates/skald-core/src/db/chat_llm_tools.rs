use sqlx::SqlitePool;

#[derive(Debug, Clone)]
pub struct LlmToolCall {
    pub id:          i64,
    pub message_id:  i64,
    pub name:        String,
    pub arguments:   Option<String>,
    pub result:      Option<String>,
    /// Result type tag: `"string"` (plain text, default) or `"json"` (structured
    /// payload, e.g. MCP `structuredContent`). Drives frontend rendering.
    pub result_type: String,
    pub status:      String,
    /// For a file-write tool: the file content **before**/**after** the write,
    /// captured at execution time so the diff renders inline in the chat card and
    /// survives a page reload (it was previously only on the transient `PendingWrite`
    /// event). `None` for non-write tools, an unreadable path, or content over the
    /// size cap (no diff shown then). Only populated by `for_message` (history).
    pub preview_old: Option<String>,
    pub preview_new: Option<String>,
    /// JSON `[{host_path, mime}]` — media files this tool produced (e.g. `read_file`
    /// on an image), to be inlined to the model as native input by the message
    /// builder. `None` for non-media tools. Only populated by `for_message`/`get`.
    pub media:       Option<String>,
}

/// Inserts a tool call in `running` state and returns its id.
/// `message_id` is the assistant `chat_history` row that triggered the call.
pub async fn append(
    pool:       &SqlitePool,
    message_id: i64,
    name:       &str,
    arguments:  &str,
) -> anyhow::Result<i64> {
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO chat_llm_tools (message_id, name, arguments, status) VALUES (?, ?, ?, 'running') RETURNING id",
    )
    .bind(message_id)
    .bind(name)
    .bind(arguments)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Marks a tool call as `pending` (waiting for explicit user approval or clarification).
/// Called just before registering an approval/clarification channel so `'pending'`
/// in the DB means "blocked on user input", not "still executing".
pub async fn set_approval_pending(pool: &SqlitePool, id: i64) -> anyhow::Result<()> {
    sqlx::query("UPDATE chat_llm_tools SET status='pending' WHERE id=?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn complete(pool: &SqlitePool, id: i64, result: &str, result_type: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE chat_llm_tools SET result = ?, result_type = ?, status = 'done' WHERE id = ?",
    )
    .bind(result)
    .bind(result_type)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Persists a file-write tool's before/after snapshot (the diff preview) on its row.
/// Both `None` is a valid no-op state (non-write tool, unreadable path, or content
/// over the size cap). Separate from [`complete`] so the status/result write and the
/// preview write stay independent, and so it can run for both the live and resume paths.
pub async fn set_preview(
    pool: &SqlitePool,
    id:   i64,
    old:  Option<&str>,
    new:  Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE chat_llm_tools SET preview_old = ?, preview_new = ? WHERE id = ?")
        .bind(old)
        .bind(new)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Persists the JSON media manifest for a tool that produced viewable media
/// (`ToolResult::Media`). Separate from [`complete`] — like [`set_preview`] — so the
/// out-of-band media write stays independent of the status/result write. Read back
/// by `for_message` so the message builder can inline the files for the model.
pub async fn set_media(pool: &SqlitePool, id: i64, media_json: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE chat_llm_tools SET media = ? WHERE id = ?")
        .bind(media_json)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn fail(pool: &SqlitePool, id: i64, error: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE chat_llm_tools SET result = ?, status = 'failed' WHERE id = ?",
    )
    .bind(error)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks a tool call as `cancelled` — stopped by the user via `/stop`.
/// Terminal and distinct from `failed`: a cancellation is deliberate, not an
/// error, and is **not** picked up by `pending_for_stack` (never re-run on
/// restart, unlike an interrupted `running` call).
pub async fn cancel(pool: &SqlitePool, id: i64, note: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE chat_llm_tools SET result = ?, status = 'cancelled' WHERE id = ?",
    )
    .bind(note)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Marks a tool call as `rejected` — denied by an approval policy or a human.
/// Terminal and distinct from `failed`: a denial is a policy decision, not an
/// error, and is not re-run on restart.
pub async fn reject(pool: &SqlitePool, id: i64, reason: &str) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE chat_llm_tools SET result = ?, status = 'rejected' WHERE id = ?",
    )
    .bind(reason)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// All `running` or `pending` tool calls for a stack frame — used to resume interrupted sessions.
/// `running`: tool was executing when the session was interrupted (re-execute).
/// `pending`: tool was waiting for explicit user approval or clarification (re-gate or re-ask).
pub async fn pending_for_stack(
    pool:             &SqlitePool,
    session_stack_id: i64,
) -> anyhow::Result<Vec<LlmToolCall>> {
    let rows = sqlx::query_as::<_, (i64, i64, String, Option<String>, Option<String>, String, String)>(
        "SELECT t.id, t.message_id, t.name, t.arguments, t.result, t.result_type, t.status
         FROM   chat_llm_tools t
         JOIN   chat_history h ON t.message_id = h.id
         WHERE  h.session_stack_id = ?
           AND  t.status IN ('running', 'pending')
         ORDER  BY t.id ASC",
    )
    .bind(session_stack_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(row_to_tool).collect())
}

/// All tool calls for a single assistant message, ordered chronologically. Unlike
/// [`pending_for_stack`], this also reads the diff-preview columns, so the history
/// projection can re-render a write's diff after a page reload.
pub async fn for_message(
    pool:       &SqlitePool,
    message_id: i64,
) -> anyhow::Result<Vec<LlmToolCall>> {
    let rows = sqlx::query_as::<_, (i64, i64, String, Option<String>, Option<String>, String, String, Option<String>, Option<String>, Option<String>)>(
        "SELECT id, message_id, name, arguments, result, result_type, status, preview_old, preview_new, media
         FROM   chat_llm_tools
         WHERE  message_id = ?
         ORDER  BY id ASC",
    )
    .bind(message_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter()
        .map(|(id, message_id, name, arguments, result, result_type, status, preview_old, preview_new, media)| {
            LlmToolCall { id, message_id, name, arguments, result, result_type, status, preview_old, preview_new, media }
        })
        .collect())
}

/// A single tool call by id, with its diff-preview columns. Backs the tool-detail
/// page (`GET /api/tools/{id}`). Returns `None` when the id is unknown in this pool.
pub async fn get(pool: &SqlitePool, id: i64) -> anyhow::Result<Option<LlmToolCall>> {
    let row = sqlx::query_as::<_, (i64, i64, String, Option<String>, Option<String>, String, String, Option<String>, Option<String>, Option<String>)>(
        "SELECT id, message_id, name, arguments, result, result_type, status, preview_old, preview_new, media
         FROM   chat_llm_tools
         WHERE  id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(id, message_id, name, arguments, result, result_type, status, preview_old, preview_new, media)| {
        LlmToolCall { id, message_id, name, arguments, result, result_type, status, preview_old, preview_new, media }
    }))
}

fn row_to_tool(
    (id, message_id, name, arguments, result, result_type, status): (
        i64, i64, String, Option<String>, Option<String>, String, String,
    ),
) -> LlmToolCall {
    // The resume path (`pending_for_stack`) never needs the diff preview or media.
    LlmToolCall { id, message_id, name, arguments, result, result_type, status, preview_old: None, preview_new: None, media: None }
}
