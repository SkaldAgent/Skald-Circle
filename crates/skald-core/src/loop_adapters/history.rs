//! `SqliteHistory` — `HistoryStore` over the EXISTING Skald tables (no
//! migration, blueprint §0/§10):
//!
//! | crate concept | Skald table |
//! |---|---|
//! | conversation `"session:{id}"` | `chat_sessions.id` (the id rides in the `ConversationId` string) |
//! | frame | `chat_sessions_stack` (`terminated_at IS NULL` = active) |
//! | message | `chat_history` (`status='failed'` = failed orphan) |
//! | tool call | `chat_llm_tools` (status strings map 1:1 on `CallState`) |
//! | summary | `chat_summaries` (`covers_up_to_message_id`) |
//!
//! The store is built on an **owner pool** (one per user, §11): all ids are
//! pool-local, so the adapter needs no user scoping. The wire tool-call id is
//! synthesized as `tc_{row_id}`, exactly like the current message builder.

use std::sync::Arc;

use agent_loop::model::Usage;
use agent_loop::store::{
    CallOutcome, CallState, FrameRecord, FrameSpec, HistoryStore, NewCall, NewMessage, NewSummary,
    Role, StoredCall, StoredMessage, StoredSummary,
};
use agent_loop::tool::ToolOutput;
use agent_loop::ids::{ConversationId, FrameId, MessageId, SummaryId, ToolCallId};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::db::{chat_history, chat_llm_tools, chat_sessions_stack, chat_summaries};

/// `HistoryStore` on a Skald owner pool.
pub struct SqliteHistory {
    pool: Arc<SqlitePool>,
}

impl SqliteHistory {
    pub fn new(pool: Arc<SqlitePool>) -> Self { Self { pool } }

    /// The conversation id of a session — the encoding, in one place.
    pub fn conversation(session_id: i64) -> ConversationId {
        ConversationId::new(format!("session:{session_id}"))
    }

    /// Parse `"session:{id}"` (the adapter's conversation encoding).
    pub fn session_id(conv: &ConversationId) -> anyhow::Result<i64> {
        conv.as_str()
            .strip_prefix("session:")
            .and_then(|s| s.parse::<i64>().ok())
            .ok_or_else(|| anyhow::anyhow!("SqliteHistory: conversation id must be \"session:<i64>\", got '{conv}'"))
    }

    fn map_role(role: Role) -> anyhow::Result<chat_history::Role> {
        match role {
            Role::User      => Ok(chat_history::Role::User),
            Role::Assistant => Ok(chat_history::Role::Assistant),
            Role::Agent     => Ok(chat_history::Role::Agent),
            // chat_history has no system role: system context is BUILT, never
            // stored. Failing loudly beats silently mis-filing a message.
            Role::System    => anyhow::bail!(
                "SqliteHistory: Role::System is not persistable — system context is not stored"
            ),
        }
    }

    fn unmap_role(role: &chat_history::Role) -> Role {
        match role {
            chat_history::Role::User      => Role::User,
            chat_history::Role::Assistant => Role::Assistant,
            chat_history::Role::Agent     => Role::Agent,
        }
    }

    fn map_state(state: CallState) -> &'static str {
        match state {
            CallState::Running       => "running",
            CallState::AwaitingHuman => "pending",
            CallState::Done          => "done",
            CallState::Failed        => "failed",
            CallState::Cancelled     => "cancelled",
            CallState::Rejected      => "rejected",
        }
    }

    fn unmap_state(status: &str) -> CallState {
        match status {
            "pending"   => CallState::AwaitingHuman,
            "done"      => CallState::Done,
            "failed"    => CallState::Failed,
            "cancelled" => CallState::Cancelled,
            "rejected"  => CallState::Rejected,
            _           => CallState::Running,
        }
    }

    fn stored_call(c: chat_llm_tools::LlmToolCall) -> StoredCall {
        let arguments: Value = c
            .arguments
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Object(Default::default()));
        // preview/media ride in `extras` (host free-form), mirroring how the
        // current loop reads them back for the history projection.
        let extras = serde_json::json!({
            "preview_old": c.preview_old,
            "preview_new": c.preview_new,
            "media":       c.media,
        });
        StoredCall {
            id: ToolCallId(c.id),
            message_id: MessageId(c.message_id),
            provider_id: format!("tc_{}", c.id),
            name: c.name,
            arguments,
            // The column holds the model's own string: the projection replays it
            // verbatim, so the prompt-cache prefix stays byte-identical (a
            // re-serialized Value would reorder the object keys).
            arguments_raw: c.arguments,
            state: Self::unmap_state(&c.status),
            result: c.result,
            result_kind: c.result_type,
            extras,
        }
    }

    fn stored_message(m: chat_history::ChatMessage, calls: Vec<StoredCall>) -> StoredMessage {
        StoredMessage {
            id: MessageId(m.id),
            role: Self::unmap_role(&m.role),
            content: m.content,
            reasoning: m.reasoning_content,
            synthetic: m.is_synthetic,
            failed: m.status == "failed",
            metadata: m.metadata.map(|meta| {
                serde_json::to_value(meta).unwrap_or(Value::Null)
            }),
            usage: Usage {
                input_tokens:  m.input_tokens.map(|n| n as u32),
                output_tokens: m.output_tokens.map(|n| n as u32),
                cache_read:    None,
                cache_write:   None,
                cost_usd:      m.cost,
                truncated:     false,
            },
            calls,
        }
    }

    async fn with_calls(&self, msgs: Vec<chat_history::ChatMessage>) -> anyhow::Result<Vec<StoredMessage>> {
        let mut out = Vec::with_capacity(msgs.len());
        for m in msgs {
            let calls = chat_llm_tools::for_message(&self.pool, m.id)
                .await?
                .into_iter()
                .map(Self::stored_call)
                .collect();
            out.push(Self::stored_message(m, calls));
        }
        Ok(out)
    }
}

#[agent_loop::async_trait]
impl HistoryStore for SqliteHistory {
    // ── frames ──

    async fn open_frame(
        &self,
        conv:   &ConversationId,
        parent: Option<FrameId>,
        spec:   FrameSpec,
    ) -> agent_loop::Result<FrameId> {
        let session_id = Self::session_id(conv)?;
        // Root frame: reuse the session's existing root stack row when present
        // (sessions are provisioned with one), create it otherwise.
        if parent.is_none()
            && let Some(root) = chat_sessions_stack::main_for_session(&self.pool, session_id).await?
        {
            return Ok(FrameId(root.id));
        }
        let frame = chat_sessions_stack::create(
            &self.pool,
            session_id,
            &spec.agent,
            spec.prompt.as_deref(),
            spec.depth as i64,
            spec.parent_call.map(|c| c.get()),
        )
        .await?;
        Ok(FrameId(frame.id))
    }

    async fn close_frame(&self, frame: FrameId) -> agent_loop::Result<()> {
        chat_sessions_stack::terminate(&self.pool, frame.get()).await?;
        Ok(())
    }

    async fn get_frame(&self, frame: FrameId) -> agent_loop::Result<Option<FrameRecord>> {
        let row = sqlx::query_as::<_, (i64, i64, String, Option<String>, i64, Option<i64>, Option<String>)>(
            "SELECT id, session_id, agent_id, agent_prompt, depth, parent_tool_call_id, terminated_at
             FROM   chat_sessions_stack
             WHERE  id = ?",
        )
        .bind(frame.get())
        .fetch_optional(&*self.pool)
        .await?;
        Ok(row.map(|(id, sid, agent, prompt, depth, parent_call, terminated)| FrameRecord {
            id: FrameId(id),
            conversation: ConversationId::new(format!("session:{sid}")),
            parent: None,
            spec: FrameSpec {
                agent,
                prompt,
                depth: depth as u32,
                parent_call: parent_call.map(ToolCallId),
                meta: Value::Null,
            },
            active: terminated.is_none(),
        }))
    }

    async fn active_frames(&self, conv: &ConversationId) -> agent_loop::Result<Vec<FrameRecord>> {
        let session_id = Self::session_id(conv)?;
        let rows = sqlx::query_as::<_, (i64, i64, String, Option<String>, i64, Option<i64>)>(
            "SELECT id, session_id, agent_id, agent_prompt, depth, parent_tool_call_id
             FROM   chat_sessions_stack
             WHERE  session_id = ? AND terminated_at IS NULL
             ORDER  BY depth ASC",
        )
        .bind(session_id)
        .fetch_all(&*self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, sid, agent, prompt, depth, parent_call)| FrameRecord {
                id: FrameId(id),
                conversation: ConversationId::new(format!("session:{sid}")),
                // The parent frame id is not stored directly (only the parent
                // tool call); recovery walks the call when it needs the link.
                parent: None,
                spec: FrameSpec {
                    agent,
                    prompt,
                    depth: depth as u32,
                    parent_call: parent_call.map(ToolCallId),
                    meta: Value::Null,
                },
                active: true,
            })
            .collect())
    }

    async fn frame_of_call(&self, id: ToolCallId) -> agent_loop::Result<Option<FrameRecord>> {
        let frame = sqlx::query_scalar::<_, i64>(
            "SELECT h.stack_id
             FROM   chat_llm_tools t
             JOIN   chat_history   h ON h.id = t.message_id
             WHERE  t.id = ?",
        )
        .bind(id.get())
        .fetch_optional(&*self.pool)
        .await?;
        match frame {
            Some(f) => self.get_frame(FrameId(f)).await,
            None    => Ok(None),
        }
    }

    async fn deepest_active(&self, conv: &ConversationId) -> agent_loop::Result<Option<FrameRecord>> {
        Ok(self
            .active_frames(conv)
            .await?
            .into_iter()
            .max_by_key(|f| f.spec.depth))
    }

    // ── messages ──

    async fn append(&self, frame: FrameId, msg: NewMessage) -> agent_loop::Result<MessageId> {
        let role = Self::map_role(msg.role)?;
        // chat_history.metadata is a typed MessageMetadata column; the crate's
        // free-form Value only round-trips when it parses back as one.
        let metadata = msg
            .metadata
            .as_ref()
            .and_then(|v| serde_json::from_value::<core_api::message_meta::MessageMetadata>(v.clone()).ok());
        let id = chat_history::append_with_metadata(
            &self.pool,
            frame.get(),
            &role,
            &msg.content,
            msg.synthetic,
            msg.reasoning.as_deref(),
            metadata.as_ref(),
        )
        .await?;
        Ok(MessageId(id))
    }

    async fn set_usage(&self, msg: MessageId, usage: &Usage) -> agent_loop::Result<()> {
        if let (Some(i), Some(o)) = (usage.input_tokens, usage.output_tokens) {
            chat_history::set_usage(&self.pool, msg.get(), i, o, 0, usage.cost_usd).await?;
        }
        Ok(())
    }

    async fn load(&self, frame: FrameId) -> agent_loop::Result<Vec<StoredMessage>> {
        let msgs = chat_history::for_stack(&self.pool, frame.get()).await?;
        self.with_calls(msgs).await
    }

    async fn load_since(&self, frame: FrameId, after: MessageId) -> agent_loop::Result<Vec<StoredMessage>> {
        let msgs = chat_history::for_stack_since(&self.pool, frame.get(), after.get()).await?;
        self.with_calls(msgs).await
    }

    async fn last(&self, frame: FrameId) -> agent_loop::Result<Option<StoredMessage>> {
        let Some(m) = chat_history::last_message_for_stack(&self.pool, frame.get()).await? else {
            return Ok(None);
        };
        Ok(self.with_calls(vec![m]).await?.into_iter().next())
    }

    async fn mark_failed(&self, msg: MessageId) -> agent_loop::Result<()> {
        chat_history::mark_failed(&self.pool, msg.get()).await?;
        Ok(())
    }

    // ── tool calls ──

    async fn append_call(&self, msg: MessageId, call: NewCall) -> agent_loop::Result<ToolCallId> {
        let args = serde_json::to_string(&call.arguments)?;
        let id = chat_llm_tools::append(&self.pool, msg.get(), &call.name, &args).await?;
        Ok(ToolCallId(id))
    }

    async fn resolve_call(&self, id: ToolCallId, outcome: &CallOutcome) -> agent_loop::Result<()> {
        let pool = &self.pool;
        match outcome {
            CallOutcome::Completed(out) => {
                chat_llm_tools::complete(pool, id.get(), &out.to_wire(), out.kind()).await?;
                if let ToolOutput::Media { refs, .. } = out {
                    let media_json = serde_json::to_string(refs)?;
                    chat_llm_tools::set_media(pool, id.get(), &media_json).await?;
                }
            }
            CallOutcome::Failed(e) => {
                chat_llm_tools::fail(pool, id.get(), e).await?;
            }
            CallOutcome::Cancelled => {
                chat_llm_tools::cancel(pool, id.get(), &outcome.result_text()).await?;
            }
            CallOutcome::Rejected { reason } => {
                chat_llm_tools::reject(pool, id.get(), reason).await?;
            }
        }
        Ok(())
    }

    async fn set_call_state(&self, id: ToolCallId, state: CallState) -> agent_loop::Result<()> {
        anyhow::ensure!(
            !state.is_terminal(),
            "set_call_state is only for Running → AwaitingHuman, not terminal {state:?}"
        );
        sqlx::query("UPDATE chat_llm_tools SET status = ? WHERE id = ?")
            .bind(Self::map_state(state))
            .bind(id.get())
            .execute(&*self.pool)
            .await?;
        Ok(())
    }

    async fn get_call(&self, id: ToolCallId) -> agent_loop::Result<Option<StoredCall>> {
        Ok(chat_llm_tools::get(&self.pool, id.get()).await?.map(Self::stored_call))
    }

    async fn set_call_extras(&self, id: ToolCallId, extras: Value) -> agent_loop::Result<()> {
        // Map the known extras onto the dedicated columns (preview, media);
        // unknown keys are dropped (the table has no generic blob).
        if extras.get("preview_old").is_some() || extras.get("preview_new").is_some() {
            let old = extras["preview_old"].as_str();
            let new = extras["preview_new"].as_str();
            chat_llm_tools::set_preview(&self.pool, id.get(), old, new).await?;
        }
        if let Some(media) = extras["media"].as_str() {
            chat_llm_tools::set_media(&self.pool, id.get(), media).await?;
        }
        Ok(())
    }

    async fn calls_in_state(&self, frame: FrameId, states: &[CallState]) -> agent_loop::Result<Vec<StoredCall>> {
        // All calls of the frame, filtered in Rust: a frame's call set is
        // bounded, and a static query keeps sqlx's dynamic-SQL audit happy.
        let rows = sqlx::query_as::<_, (i64, i64, String, Option<String>, Option<String>, String, String)>(
            "SELECT t.id, t.message_id, t.name, t.arguments, t.result, t.result_type, t.status
             FROM   chat_llm_tools t
             JOIN   chat_history h ON t.message_id = h.id
             WHERE  h.session_stack_id = ?
             ORDER  BY t.id ASC",
        )
        .bind(frame.get())
        .fetch_all(&*self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(id, message_id, name, arguments, result, result_type, status)| {
                Self::stored_call(chat_llm_tools::LlmToolCall {
                    id,
                    message_id,
                    name,
                    arguments,
                    result,
                    result_type,
                    status,
                    preview_old: None,
                    preview_new: None,
                    media: None,
                })
            })
            .filter(|c| states.contains(&c.state))
            .collect())
    }

    // ── summaries ──

    async fn save_summary(&self, frame: FrameId, s: NewSummary) -> agent_loop::Result<SummaryId> {
        let id = chat_summaries::save(&self.pool, frame.get(), &s.text, s.covered_up_to.get()).await?;
        Ok(SummaryId(id))
    }

    async fn latest_summary(&self, frame: FrameId) -> agent_loop::Result<Option<StoredSummary>> {
        let Some(s) = chat_summaries::latest_for_stack(&self.pool, frame.get()).await? else {
            return Ok(None);
        };
        Ok(Some(StoredSummary {
            id: SummaryId(s.id),
            text: s.content,
            covered_up_to: MessageId(s.covers_up_to_message_id),
        }))
    }
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

    async fn setup(tag: &str) -> (Arc<SqlitePool>, SqliteHistory, ConversationId, String) {
        let path = temp_db_path(tag);
        let pool = Arc::new(crate::db::init_system_pool(&path).await.unwrap());
        sqlx::query("INSERT INTO chat_sessions (id) VALUES (1)")
            .execute(&*pool)
            .await
            .unwrap();
        // The session's root frame (created at provisioning time in production).
        chat_sessions_stack::create(&pool, 1, "assistant", None, 0, None).await.unwrap();
        let store = SqliteHistory::new(pool.clone());
        (pool, store, ConversationId::new("session:1"), path)
    }

    #[tokio::test]
    async fn frames_open_reuse_root_and_close() {
        let (pool, store, conv, path) = setup("hist-frames").await;

        // Root: reuses the provisioned root frame.
        let root = store.open_frame(&conv, None, FrameSpec::root("assistant")).await.unwrap();
        // Child: creates a new frame at depth 1.
        let child = store
            .open_frame(&conv, Some(root), FrameSpec {
                agent: "task".into(),
                prompt: Some("do a thing".into()),
                depth: 1,
                parent_call: None,
                meta: Value::Null,
            })
            .await
            .unwrap();
        assert_ne!(root, child);

        let active = store.active_frames(&conv).await.unwrap();
        assert_eq!(active.len(), 2);
        assert_eq!(store.deepest_active(&conv).await.unwrap().unwrap().id, child);

        store.close_frame(child).await.unwrap();
        assert!(store.deepest_active(&conv).await.unwrap().unwrap().spec.depth == 0);

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn messages_calls_and_states_round_trip() {
        let (pool, store, conv, path) = setup("hist-msgs").await;
        let frame = store.open_frame(&conv, None, FrameSpec::root("assistant")).await.unwrap();

        store.append(frame, NewMessage::user("hi")).await.unwrap();
        let asst = store.append(frame, NewMessage::assistant("calling", Some("thinking…".into()))).await.unwrap();
        let call = store
            .append_call(asst, NewCall::new("read_file", serde_json::json!({"path": "a.txt"})))
            .await
            .unwrap();

        // Running → AwaitingHuman (the only legal set_call_state).
        store.set_call_state(call, CallState::AwaitingHuman).await.unwrap();
        assert!(store.set_call_state(call, CallState::Done).await.is_err());

        store
            .resolve_call(call, &CallOutcome::Completed(ToolOutput::Text("file contents".into())))
            .await
            .unwrap();

        let history = store.load(frame).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].reasoning.as_deref(), Some("thinking…"));
        assert_eq!(history[1].calls.len(), 1);
        let c = &history[1].calls[0];
        assert_eq!(c.state, CallState::Done);
        assert_eq!(c.result.as_deref(), Some("file contents"));
        assert_eq!(c.provider_id, format!("tc_{}", c.id.get()));
        assert_eq!(c.arguments["path"], serde_json::json!("a.txt"));

        let done = store.calls_in_state(frame, &[CallState::Done]).await.unwrap();
        assert_eq!(done.len(), 1);

        // Orphan marking drops the message from the projection.
        store.mark_failed(history[0].id).await.unwrap();
        assert_eq!(store.load(frame).await.unwrap().len(), 1);

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn summaries_round_trip() {
        let (pool, store, conv, path) = setup("hist-sum").await;
        let frame = store.open_frame(&conv, None, FrameSpec::root("assistant")).await.unwrap();

        let m1 = store.append(frame, NewMessage::user("old")).await.unwrap();
        store.append(frame, NewMessage::assistant("answer", None)).await.unwrap();
        let m3 = store.append(frame, NewMessage::user("new")).await.unwrap();

        store
            .save_summary(frame, NewSummary { text: "covered".into(), covered_up_to: m1 })
            .await
            .unwrap();
        let latest = store.latest_summary(frame).await.unwrap().unwrap();
        assert_eq!(latest.text, "covered");
        assert_eq!(latest.covered_up_to, m1);

        let since = store.load_since(frame, latest.covered_up_to).await.unwrap();
        assert_eq!(since.len(), 2);
        assert_eq!(since[1].id, m3);

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn system_role_is_rejected() {
        let (pool, store, conv, path) = setup("hist-sys").await;
        let frame = store.open_frame(&conv, None, FrameSpec::root("assistant")).await.unwrap();
        let msg = NewMessage {
            role: Role::System,
            content: "nope".into(),
            synthetic: true,
            reasoning: None,
            metadata: None,
        };
        assert!(store.append(frame, msg).await.is_err());
        pool.close().await;
        cleanup(&path);
    }
}
