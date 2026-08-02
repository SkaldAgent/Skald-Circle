use std::collections::HashMap;
use std::sync::Arc;

use core_api::user_fs::{SharedFs, UserFs};

use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::approval::ApprovalManager;
use crate::chat_event_bus::ChatEventBus;
use crate::clarification::ClarificationManager;
use crate::compactor::ContextCompactor;
use crate::config::DatetimeConfig;
use crate::db::{chat_sessions, chat_sessions_stack};
use crate::llm::LlmManager;
use crate::loop_adapters::runtime::{LoopConfig, UserLoopRuntime};
use crate::mcp::McpProvider;
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
    /// `ToolContext` (blueprint §6). A shared swappable cell so a shared-folder
    /// membership change ([`refresh_fs`](Self::refresh_fs)) reaches live sessions.
    user_fs:               SharedFs,
    llm_manager:           Arc<LlmManager>,
    max_tool_rounds:       usize,
    tools:                 Arc<ToolRegistry>,
    /// The MCP tools visible to this owner: the access-filtered global runtime
    /// unioned with their per-user runtime (blueprint §7), behind one trait.
    mcp:                   Arc<dyn McpProvider>,
    approval:              Arc<ApprovalManager>,
    clarification:         Arc<ClarificationManager>,
    event_bus:             Arc<ChatEventBus>,
    memory_manager:          Arc<MemoryManager>,
    image_generator_manager: Arc<ImageGeneratorManager>,
    /// Shared compactor instance. Always present — manual `/compact` needs no
    /// configuration; `CompactionConfig::threshold_tokens` arms the automatic
    /// trigger on top of it.
    compactor:               Arc<ContextCompactor>,
    run_context_manager:     Arc<RunContextManager>,
    /// This user's loop stack (blueprint D12): built once here and shared by
    /// every session of the owner, so the manager keeps a global view of what
    /// is running and a turn only contributes its own parameters.
    loop_runtime:            Arc<UserLoopRuntime>,
    active:                Mutex<HashMap<i64, Arc<ChatSessionHandler>>>,
}

impl ChatSessionManager {
    pub fn new(
        db:                    Arc<SqlitePool>,
        shared_pool:           Arc<SqlitePool>,
        user_id:               String,
        user_fs:               SharedFs,
        llm_manager:           Arc<LlmManager>,
        max_history_messages:  Option<usize>,
        max_tool_rounds:       usize,
        max_parallel_subagents: usize,
        max_tool_result_chars: Option<usize>,
        datetime_config:       DatetimeConfig,
        tools:                 Arc<ToolRegistry>,
        mcp:                   Arc<dyn McpProvider>,
        approval:              Arc<ApprovalManager>,
        clarification:         Arc<ClarificationManager>,
        event_bus:             Arc<ChatEventBus>,
        memory_manager:          Arc<MemoryManager>,
        image_generator_manager: Arc<ImageGeneratorManager>,
        compactor:               Arc<ContextCompactor>,
        run_context_manager:     Arc<RunContextManager>,
        tool_discovery:          Arc<ToolDiscovery>,
    ) -> anyhow::Result<Self> {
        let loop_runtime = UserLoopRuntime::build(
            db.clone(),
            shared_pool.clone(),
            user_id.clone(),
            user_fs.clone(),
            tools.clone(),
            mcp.clone(),
            llm_manager.clone(),
            approval.clone(),
            clarification.clone(),
            tool_discovery.clone(),
            LoopConfig {
                max_rounds:            max_tool_rounds,
                max_parallel_calls:    max_parallel_subagents,
                max_history_messages,
                max_tool_result_chars,
                // The window yields to the *automatic* compactor, not to its mere
                // existence: manual `/compact` alone must not silently disable a
                // configured message cap.
                auto_compaction_enabled: compactor.auto_enabled(),
                datetime:              datetime_config.clone(),
                max_agent_depth:       crate::session::handler::MAX_AGENT_DEPTH as u32,
            },
        )?;

        Ok(Self {
            db,
            shared_pool,
            user_id,
            user_fs,
            llm_manager,
            max_tool_rounds,
            tools,
            mcp,
            approval,
            clarification,
            event_bus,
            memory_manager,
            image_generator_manager,
            compactor,
            run_context_manager,
            loop_runtime,
            active: Mutex::new(HashMap::new()),
        })
    }

    pub fn llm_manager(&self) -> Arc<LlmManager> {
        Arc::clone(&self.llm_manager)
    }

    pub fn run_context_manager(&self) -> Arc<RunContextManager> {
        Arc::clone(&self.run_context_manager)
    }

    /// This owner's loop stack (blueprint D12) — the wiring hands it the pieces
    /// that only exist after the session manager does (the `TaskManager`).
    pub fn loop_runtime(&self) -> &Arc<UserLoopRuntime> {
        &self.loop_runtime
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
        // The root stack frame runs the session's own entry agent — not a hardcoded
        // default. Using the wrong id here would silently run that agent's prompt
        // regardless of what the session was created with (llm_loop resolves the
        // prompt from `config.agent_id`, which comes from the stack frame).
        let stack   = chat_sessions_stack::create(
            &self.db, session.id, agent_id, None, 0, None,
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

        // The persisted group is **advisory**: re-check it against the owner's current
        // role, so a group revoked since the session last ran cannot be replayed from
        // the row. Every load goes through here — restart, re-login, new handler — so
        // correctness does not depend on anyone having pushed a notification.
        let run_context = crate::run_context::reconcile_group_for_user(
            &self.shared_pool,
            &self.user_id,
            session.run_context.as_deref().and_then(RunContext::from_db),
        )
        .await;

        let handler = Arc::new(ChatSessionHandler::new(
            session_id,
            self.db.clone(),
            self.shared_pool.clone(),
            self.user_id.clone(),
            self.user_fs.clone(),
            Arc::clone(&self.llm_manager),
            self.max_tool_rounds,
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
            Arc::clone(&self.loop_runtime),
        ));

        self.active.lock().await.insert(session_id, handler.clone());
        Ok(handler)
    }

    /// Swaps in a refreshed filesystem view for this owner (blueprint §6 remount).
    /// Every live session's handler shares the same [`SharedFs`] cell, so the new
    /// membership reaches each on its next tool call — no handler eviction, no
    /// cross-session race.
    pub fn refresh_fs(&self, fs: UserFs) {
        self.user_fs.store(fs);
    }

    /// Re-checks every **live** handler's security group against the owner's current
    /// role, degrading any the role no longer allows (see
    /// [`crate::run_context::reconcile_group_for_user`]).
    ///
    /// [`get_or_create_handler`](Self::get_or_create_handler) already reconciles on
    /// load, which covers every future session; this covers the sessions that are
    /// *already* open, whose handler holds its run-context in RAM and would otherwise
    /// keep the revoked group until the process restarts. Both the row and the live
    /// handler are updated, so the change survives and the UI reads the truth.
    ///
    /// Returns `(source, effective group)` for each session that actually changed —
    /// the caller broadcasts `SecurityGroupSelected` so open tabs re-sync their pill.
    pub async fn revalidate_security_groups(&self) -> Vec<(String, String)> {
        let handlers: Vec<_> = self.active.lock().await
            .iter().map(|(id, h)| (*id, Arc::clone(h))).collect();

        let mut changed = Vec::new();
        for (session_id, handler) in handlers {
            let before = handler.run_context.read().await.clone();
            let before_group = before.as_ref().and_then(|rc| rc.tool_group_id().map(str::to_string));
            let after = crate::run_context::reconcile_group_for_user(
                &self.shared_pool, &self.user_id, before,
            ).await;
            let after_group = after.as_ref().and_then(|rc| rc.tool_group_id().map(str::to_string));
            if before_group == after_group {
                continue;
            }

            if let Err(e) = chat_sessions::set_run_context(
                &self.db, session_id, after.as_ref().map(|rc| rc.to_db()).as_deref(),
            ).await {
                // The in-RAM update below still takes effect for this process; the
                // reconcile on next load would redo the degrade anyway.
                tracing::warn!(session = session_id, error = %e,
                    "failed to persist a degraded security group");
            }
            handler.set_run_context(after).await;
            changed.push((
                handler.source.clone(),
                after_group.unwrap_or_else(|| crate::run_context::DEFAULT_GROUP_ID.to_string()),
            ));
        }
        changed
    }
}
