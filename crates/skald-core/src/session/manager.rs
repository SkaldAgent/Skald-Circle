use std::collections::HashMap;
use std::sync::Arc;

use core_api::user_fs::UserFs;

use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::approval::ApprovalManager;
use crate::chat_event_bus::ChatEventBus;
use crate::clarification::ClarificationManager;
use crate::compactor::ContextCompactor;
use crate::config::DatetimeConfig;
use crate::db::{chat_sessions, chat_sessions_stack};
use crate::llm::LlmManager;
use crate::mcp::McpManager;
use crate::image_generate::ImageGeneratorManager;
use crate::memory::MemoryManager;
use crate::run_context::{RunContext, RunContextManager};
use crate::tool_discovery::ToolDiscovery;
use crate::tools::ToolRegistry;

use super::handler::ChatSessionHandler;

pub struct ChatSessionManager {
    db:                    Arc<SqlitePool>,
    /// The shared (`system.db`) pool, threaded to each handler for cross-owner
    /// reads such as injecting `shared-memory/` notes.
    shared_pool:           Arc<SqlitePool>,
    user_id:               String,
    /// The owner's filesystem view, threaded to each handler and on into every
    /// `ToolContext` (blueprint §6).
    user_fs:               Arc<UserFs>,
    llm_manager:           Arc<LlmManager>,
    max_history_messages:  usize,
    max_tool_rounds:       usize,
    max_parallel_subagents: usize,
    max_tool_result_chars: Option<usize>,
    datetime_config:       DatetimeConfig,
    tools:                 Arc<ToolRegistry>,
    mcp:                   Arc<McpManager>,
    approval:              Arc<ApprovalManager>,
    clarification:         Arc<ClarificationManager>,
    event_bus:             Arc<ChatEventBus>,
    memory_manager:          Arc<MemoryManager>,
    image_generator_manager: Arc<ImageGeneratorManager>,
    /// Shared compactor instance, `None` when compaction is disabled.
    compactor:               Option<Arc<ContextCompactor>>,
    run_context_manager:     Arc<RunContextManager>,
    /// Shared tool-discovery recorder, passed to every handler so each turn can
    /// register the tools it actually offers to the LLM (see `ToolDiscovery`).
    tool_discovery:          Arc<ToolDiscovery>,
    active:                Mutex<HashMap<i64, Arc<ChatSessionHandler>>>,
}

impl ChatSessionManager {
    pub fn new(
        db:                    Arc<SqlitePool>,
        shared_pool:           Arc<SqlitePool>,
        user_id:               String,
        user_fs:               Arc<UserFs>,
        llm_manager:           Arc<LlmManager>,
        max_history_messages:  usize,
        max_tool_rounds:       usize,
        max_parallel_subagents: usize,
        max_tool_result_chars: Option<usize>,
        datetime_config:       DatetimeConfig,
        tools:                 Arc<ToolRegistry>,
        mcp:                   Arc<McpManager>,
        approval:              Arc<ApprovalManager>,
        clarification:         Arc<ClarificationManager>,
        event_bus:             Arc<ChatEventBus>,
        memory_manager:          Arc<MemoryManager>,
        image_generator_manager: Arc<ImageGeneratorManager>,
        compactor:               Option<Arc<ContextCompactor>>,
        run_context_manager:     Arc<RunContextManager>,
        tool_discovery:          Arc<ToolDiscovery>,
    ) -> Self {
        Self {
            db,
            shared_pool,
            user_id,
            user_fs,
            llm_manager,
            max_history_messages,
            max_tool_rounds,
            max_parallel_subagents,
            max_tool_result_chars,
            datetime_config,
            tools,
            mcp,
            approval,
            clarification,
            event_bus,
            memory_manager,
            image_generator_manager,
            compactor,
            run_context_manager,
            tool_discovery,
            active: Mutex::new(HashMap::new()),
        }
    }

    pub fn llm_manager(&self) -> Arc<LlmManager> {
        Arc::clone(&self.llm_manager)
    }

    pub fn run_context_manager(&self) -> Arc<RunContextManager> {
        Arc::clone(&self.run_context_manager)
    }

    /// Returns the live handler for `session_id` if it is currently loaded,
    /// without creating a new one. Used by the API for in-place updates.
    pub async fn active_handler(&self, session_id: i64) -> Option<Arc<ChatSessionHandler>> {
        self.active.lock().await.get(&session_id).cloned()
    }

    pub async fn create_session(
        &self,
        agent_id:       &str,
        source:         &str,
        is_interactive: bool,
        is_ephemeral:   bool,
        run_context:    Option<&RunContext>,
    ) -> anyhow::Result<(i64, i64)> {
        let session = chat_sessions::create(&self.db, agent_id, source, is_interactive, is_ephemeral).await?;
        // Persist the RunContext at creation time so it is present before any handler
        // is constructed (get_or_create_handler reads it once at construction).
        if let Some(rc) = run_context {
            chat_sessions::set_run_context(&self.db, session.id, Some(&rc.to_db())).await?;
        }
        let stack   = chat_sessions_stack::create(
            &self.db, session.id, "main", None, 0, None,
        ).await?;
        Ok((session.id, stack.id))
    }

    /// Cancel the in-flight turn for `session_id` and clean up any pending
    /// approvals and clarifications so their blocking awaits unblock immediately.
    /// No-op if no handler is active for the session.
    pub async fn cancel_session(&self, session_id: i64) {
        let handler = self.active.lock().await.get(&session_id).cloned();
        if let Some(h) = handler {
            h.cancel();
            h.cancel_pending_approvals().await;
            h.cancel_pending_questions().await;
        }
    }

    pub async fn get_or_create_handler(
        &self,
        session_id: i64,
    ) -> anyhow::Result<Arc<ChatSessionHandler>> {
        {
            let active = self.active.lock().await;
            if let Some(h) = active.get(&session_id) {
                return Ok(h.clone());
            }
        }

        let session = chat_sessions::find_by_id(&self.db, session_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("session {session_id} not found"))?;

        let run_context = session.run_context.as_deref().and_then(RunContext::from_db);

        let handler = Arc::new(ChatSessionHandler::new(
            session_id,
            self.db.clone(),
            self.shared_pool.clone(),
            self.user_id.clone(),
            Arc::clone(&self.user_fs),
            Arc::clone(&self.llm_manager),
            self.max_history_messages,
            self.max_tool_rounds,
            self.max_parallel_subagents,
            self.max_tool_result_chars,
            self.datetime_config.clone(),
            session.agent_id,
            session.source,
            session.is_interactive,
            session.is_ephemeral,
            self.tools.clone(),
            self.mcp.clone(),
            Arc::clone(&self.approval),
            Arc::clone(&self.clarification),
            Arc::clone(&self.event_bus),
            Arc::clone(&self.memory_manager),
            Arc::clone(&self.image_generator_manager),
            self.compactor.clone(),
            run_context,
            Arc::clone(&self.tool_discovery),
        ));

        self.active.lock().await.insert(session_id, handler.clone());
        Ok(handler)
    }
}
