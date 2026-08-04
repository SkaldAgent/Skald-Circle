use sqlx::SqlitePool;

pub struct ChatSession {
    pub id:              i64,
    pub source:          String,
    pub agent_id:        String,
    /// True when a real user is actively participating (web, telegram).
    /// False for fully automated sessions (cron, event-triage).
    pub is_interactive:  bool,
    /// True for short-lived task sessions (cron, event-triage) with no long-term
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

/// One conversation the copilot keeps as a tab.
pub struct OpenSession {
    pub id:     i64,
    pub source: String,
    /// User-facing name, when one has been set. Nothing writes it yet — the column
    /// predates the tab bar, which falls back to the source's own label.
    pub title:  Option<String>,
}

/// Show or hide a conversation in the copilot's tab bar.
///
/// `chat_sessions` lives in the caller's own encrypted file, so addressing a
/// session by id is already scoped to its owner: an id from another user's pool
/// simply isn't there, and the update matches no row.
pub async fn set_open(pool: &SqlitePool, id: i64, open: bool) -> anyhow::Result<()> {
    sqlx::query("UPDATE chat_sessions SET is_open = ? WHERE id = ?")
        .bind(open as i64)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Rename a conversation. An empty title is stored as `NULL`, so clearing the
/// box gives back the automatic label rather than a blank tab.
pub async fn set_title(pool: &SqlitePool, id: i64, title: Option<&str>) -> anyhow::Result<()> {
    let title = title.map(str::trim).filter(|t| !t.is_empty());
    sqlx::query("UPDATE chat_sessions SET title = ? WHERE id = ?")
        .bind(title)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The tabs to restore, in creation order so the bar keeps a stable layout.
pub async fn list_open(pool: &SqlitePool) -> anyhow::Result<Vec<OpenSession>> {
    let rows = sqlx::query_as::<_, (i64, String, Option<String>)>(
        "SELECT id, source, title FROM chat_sessions WHERE is_open = 1 ORDER BY id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, source, title)| OpenSession { id, source, title })
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn owner_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::create_owner_tables(&pool).await.unwrap();
        pool
    }

    /// The property the `DEFAULT 0` exists for: a session is *not* a tab until the
    /// copilot says so. Every `/new` leaves its predecessor behind and every
    /// system-agent pass mints one, so the opposite default would restore a bar
    /// full of conversations nobody asked to see.
    #[tokio::test]
    async fn a_session_is_not_a_tab_until_it_is_opened() {
        let pool = owner_pool().await;
        let a = create(&pool, "assistant", "web",       true, false).await.unwrap();
        let b = create(&pool, "assistant", "project-1", true, false).await.unwrap();
        assert!(list_open(&pool).await.unwrap().is_empty());

        set_open(&pool, b.id, true).await.unwrap();
        let open = list_open(&pool).await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, b.id);
        assert_eq!(open[0].source, "project-1");
        assert!(open[0].title.is_none(), "nothing writes titles yet");

        // Closing a tab is not deleting a conversation.
        set_open(&pool, b.id, false).await.unwrap();
        assert!(list_open(&pool).await.unwrap().is_empty());
        assert!(find_by_id(&pool, b.id).await.unwrap().is_some());
        assert!(find_by_id(&pool, a.id).await.unwrap().is_some());
    }

    /// One source, two open conversations — the shape the copilot's `+` produces
    /// and the one the old per-source model could not express. Order is by id, so
    /// the bar lays out the same way on every device.
    #[tokio::test]
    async fn a_source_can_hold_several_open_conversations() {
        let pool = owner_pool().await;
        let mut ids = Vec::new();
        for _ in 0..3 {
            let s = create(&pool, "assistant", "web", true, false).await.unwrap();
            set_open(&pool, s.id, true).await.unwrap();
            ids.push(s.id);
        }
        let open = list_open(&pool).await.unwrap();
        assert_eq!(open.iter().map(|s| s.id).collect::<Vec<_>>(), ids);
    }

    /// Clearing the name gives back the automatic label instead of a blank tab, so
    /// the rename box is also how a rename is undone. Whitespace counts as empty.
    #[tokio::test]
    async fn an_empty_title_clears_the_name() {
        let pool = owner_pool().await;
        let s = create(&pool, "assistant", "web", true, false).await.unwrap();
        set_open(&pool, s.id, true).await.unwrap();

        set_title(&pool, s.id, Some("  Trip planning  ")).await.unwrap();
        assert_eq!(list_open(&pool).await.unwrap()[0].title.as_deref(), Some("Trip planning"));

        set_title(&pool, s.id, Some("   ")).await.unwrap();
        assert!(list_open(&pool).await.unwrap()[0].title.is_none());

        set_title(&pool, s.id, Some("Named again")).await.unwrap();
        set_title(&pool, s.id, None).await.unwrap();
        assert!(list_open(&pool).await.unwrap()[0].title.is_none());
    }
}
