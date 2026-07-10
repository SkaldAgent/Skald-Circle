use sqlx::SqlitePool;

pub struct ChatSession {
    pub id:              i64,
    pub source:          String,
    pub agent_id:        String,
    /// True when a real user is actively participating (web, telegram).
    /// False for fully automated sessions (cron, tic).
    pub is_interactive:  bool,
    /// True for short-lived task sessions (cron, tic) with no long-term
    /// conversational value. May be used to skip memory / analytics sinks.
    pub is_ephemeral:    bool,
    /// Optional RunContext JSON blob assigned to this session.
    /// `None` resolves to the implicit "default" run_context at runtime.
    pub run_context:     Option<String>,
}

pub async fn create(
    pool:           &SqlitePool,
    agent_id:       &str,
    source:         &str,
    is_interactive: bool,
    is_ephemeral:   bool,
) -> anyhow::Result<ChatSession> {
    let id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO chat_sessions (source, agent_id, is_interactive, is_ephemeral)
         VALUES (?, ?, ?, ?) RETURNING id",
    )
    .bind(source)
    .bind(agent_id)
    .bind(is_interactive as i64)
    .bind(is_ephemeral as i64)
    .fetch_one(pool)
    .await?;

    Ok(ChatSession {
        id,
        source:         source.to_string(),
        agent_id:       agent_id.to_string(),
        is_interactive,
        is_ephemeral,
        run_context: None,
    })
}

pub async fn set_run_context(
    pool:        &SqlitePool,
    id:          i64,
    run_context: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE chat_sessions SET run_context = ? WHERE id = ?")
        .bind(run_context)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn find_by_id(pool: &SqlitePool, id: i64) -> anyhow::Result<Option<ChatSession>> {
    let row = sqlx::query_as::<_, (i64, String, String, bool, bool, Option<String>)>(
        "SELECT id, source, agent_id, is_interactive, is_ephemeral, run_context
         FROM chat_sessions WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|(id, source, agent_id, is_interactive, is_ephemeral, run_context)| ChatSession {
        id,
        source,
        agent_id,
        is_interactive,
        is_ephemeral,
        run_context,
    }))
}
