//! Skald's side of the crate's built-in tools: the `HumanChannel`
//! (clarification manager + interactive `AgentQuestion`), scratchpad/todos
//! tools, and the legacy-name aliases (`execute_task` sync/async composition,
//! `ask_user_clarification`, interface tools).

use std::sync::Arc;

use agent_loop::async_trait;
use agent_loop::delegate::DelegateTool;
use agent_loop::events::{EventSink, LoopEvent};
use agent_loop::human::{HumanChannel, HumanGone, Question};
use agent_loop::tool::{Tool, ToolCtx, ToolFailure, ToolOutput};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::clarification::ClarificationManager;
use core_api::interface_tool::ToolFuture;

// ── SkaldHumanChannel ────────────────────────────────────────────────────────

/// The `ask_user` backend: registers in `ClarificationManager` (so the
/// question lands in the Inbox for EVERY session kind) and, for interactive
/// sessions, also emits `AgentQuestion` inline in the chat (via
/// `LoopEvent::Host`). Port of `dispatch_ask_user_clarification`.
pub struct SkaldHumanChannel {
    clarification: Arc<ClarificationManager>,
    session_id:    i64,
    agent_id:      String,
    source:        String,
    is_interactive: bool,
    context_label: Arc<std::sync::RwLock<Option<String>>>,
}

impl SkaldHumanChannel {
    pub fn new(
        clarification: Arc<ClarificationManager>,
        session_id:    i64,
        agent_id:      impl Into<String>,
        source:        impl Into<String>,
        is_interactive: bool,
        context_label: Arc<std::sync::RwLock<Option<String>>>,
    ) -> Self {
        Self {
            clarification,
            session_id,
            agent_id: agent_id.into(),
            source: source.into(),
            is_interactive,
            context_label,
        }
    }
}

#[async_trait]
impl HumanChannel for SkaldHumanChannel {
    async fn ask(&self, q: Question, events: &EventSink) -> Result<String, HumanGone> {
        let label = self.context_label.read().ok().and_then(|g| g.clone());
        let (request_id, rx) = self
            .clarification
            .register(
                self.session_id,
                &self.agent_id,
                &self.source,
                label.as_deref(),
                &q.title,
                &q.question,
                q.suggested.clone(),
            )
            .await;

        if self.is_interactive {
            events.emit(q.frame, None, LoopEvent::Host(json!({
                "type":              "agent_question",
                "request_id":        request_id,
                "tool_call_id":      q.call.get(),
                "title":             q.title,
                "question":          q.question,
                "suggested_answers": q.suggested,
            })));
        }

        // The answer arrives via WS (resolve_question) or the Inbox REST. A
        // session-wide cancel (WS drop) closes the channel → HumanGone → the
        // tool suspends and the call stays pending for resume.
        rx.await.map_err(|_| HumanGone)
    }
}

// ── UpdateScratchpadTool ─────────────────────────────────────────────────────

/// The session-scoped shared blackboard (port of `dispatch_update_scratchpad`).
pub struct UpdateScratchpadTool {
    pool: Arc<SqlitePool>,
    sid:  i64,
}

impl UpdateScratchpadTool {
    pub fn new(pool: Arc<SqlitePool>, sid: i64) -> Self { Self { pool, sid } }
}

#[async_trait]
impl Tool for UpdateScratchpadTool {
    fn name(&self) -> &str { crate::tools::tool_names::UPDATE_SCRATCHPAD }

    fn definition(&self) -> Value {
        crate::session::handler::update_scratchpad_tool_def()
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        let key   = args["key"].as_str().unwrap_or("").to_string();
        let value = args["value"].as_str().unwrap_or("").to_string();
        crate::db::scratchpad::upsert(&self.pool, self.sid, &key, &value)
            .await
            .map(|_| ToolOutput::Text(format!("Scratchpad updated: {key}")))
            .map_err(|e| ToolFailure::Failed(e.to_string()))
    }
}

// ── WriteTodosTool ───────────────────────────────────────────────────────────

/// Stateless checklist echo (port of `dispatch_write_todos`).
pub struct WriteTodosTool;

#[async_trait]
impl Tool for WriteTodosTool {
    fn name(&self) -> &str { crate::tools::tool_names::WRITE_TODOS }

    fn definition(&self) -> Value {
        crate::session::handler::write_todos_tool_def()
    }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        let items = args["todos"].as_array().ok_or_else(|| {
            ToolFailure::Failed("`write_todos` requires a `todos` array. Re-send the full list, e.g. [{\"content\":\"...\",\"status\":\"pending\"}].".into())
        })?;
        if items.is_empty() {
            return Err(ToolFailure::Failed("`todos` is empty — send at least one item, or omit the call entirely.".into()));
        }

        let mut lines = Vec::with_capacity(items.len());
        let (mut done, mut active, mut pending) = (0usize, 0usize, 0usize);
        for item in items {
            let content = item["content"].as_str().unwrap_or("").trim();
            if content.is_empty() {
                continue;
            }
            let marker = match item["status"].as_str() {
                Some("completed")   => { done   += 1; "x" }
                Some("in_progress") => { active += 1; "~" }
                _                   => { pending += 1; " " }
            };
            lines.push(format!("[{marker}] {content}"));
        }
        if lines.is_empty() {
            return Err(ToolFailure::Failed("No valid todo items (every `content` was empty).".into()));
        }

        Ok(ToolOutput::Text(format!(
            "Todo list ({total}): {done} done, {active} in progress, {pending} pending\n{body}",
            total = lines.len(),
            body  = lines.join("\n"),
        )))
    }
}

// ── SkaldAskUserTool ─────────────────────────────────────────────────────────

/// The legacy `ask_user_clarification`: the crate's `AskUserTool` mechanics
/// (AwaitingHuman + Suspend) with Skald's exact legacy definition.
pub struct SkaldAskUserTool {
    inner: agent_loop::human::AskUserTool,
}

impl SkaldAskUserTool {
    pub fn new(channel: Arc<dyn HumanChannel>, store: Arc<dyn agent_loop::store::HistoryStore>) -> Self {
        Self {
            inner: agent_loop::human::AskUserTool::new(channel, store)
                .with_name(crate::tools::tool_names::ASK_USER_CLARIFICATION),
        }
    }
}

#[async_trait]
impl Tool for SkaldAskUserTool {
    fn name(&self) -> &str { crate::tools::tool_names::ASK_USER_CLARIFICATION }

    fn definition(&self) -> Value {
        crate::session::handler::ask_user_clarification_tool_def()
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        self.inner.call(args, ctx).await
    }
}

// ── ExecuteTaskAliasTool ─────────────────────────────────────────────────────

/// The legacy `execute_task`, split by what the mode actually is.
///
/// `sync` and `async` are **delegation** — one agent handing work to another —
/// so both go to the crate's `DelegateTool` (which runs the child in place, or
/// submits it to the async executor). `cron` is **scheduling**: it creates a
/// recurring job and delegates nothing, so it stays on the interface-tool
/// handler that owns the schedule. Without that handler (a non-interactive
/// session, where cron was never offered) the mode is refused.
pub struct ExecuteTaskAliasTool {
    delegate:     DelegateTool,
    definition:   Value,
    cron_handler: Option<Arc<dyn Fn(Value) -> ToolFuture + Send + Sync>>,
}

impl ExecuteTaskAliasTool {
    pub fn new(
        delegate:     DelegateTool,
        definition:   Value,
        cron_handler: Option<Arc<dyn Fn(Value) -> ToolFuture + Send + Sync>>,
    ) -> Self {
        Self { delegate, definition, cron_handler }
    }
}

#[async_trait]
impl Tool for ExecuteTaskAliasTool {
    fn name(&self) -> &str { crate::tools::tool_names::EXECUTE_TASK }

    fn definition(&self) -> Value { self.definition.clone() }

    fn concurrency_safe(&self, args: &Value) -> bool {
        // Only a sync delegate is a plain "slow tool" the fan-out may batch.
        !matches!(args["mode"].as_str(), Some("async") | Some("cron"))
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        if args["mode"].as_str() == Some("cron") {
            let Some(handler) = &self.cron_handler else {
                return Err(ToolFailure::Failed(
                    "execute_task: cron mode is not available in this session".into(),
                ));
            };
            return handler(args)
                .await
                .map(ToolOutput::Text)
                .map_err(|e| ToolFailure::Failed(e.to_string()));
        }
        self.delegate.call(args, ctx).await
    }
}

// ── LegacyInterfaceTool ──────────────────────────────────────────────────────

/// Wraps a ChatHub-provided `InterfaceTool` (definition + handler closure) as
/// a crate-native tool — interface tools keep their exact legacy behavior
/// during the migration.
pub struct LegacyInterfaceTool {
    definition: Value,
    handler:    Arc<dyn Fn(Value) -> ToolFuture + Send + Sync>,
}

impl LegacyInterfaceTool {
    pub fn new(it: core_api::interface_tool::InterfaceTool) -> Self {
        Self { definition: it.definition, handler: it.handler }
    }
}

#[async_trait]
impl Tool for LegacyInterfaceTool {
    fn name(&self) -> &str {
        self.definition["function"]["name"].as_str().unwrap_or("")
    }

    fn definition(&self) -> Value { self.definition.clone() }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        (self.handler)(args)
            .await
            .map(ToolOutput::Text)
            .map_err(|e| ToolFailure::Failed(e.to_string()))
    }
}
