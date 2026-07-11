//! Per-user runtime context (blueprint §9 / §11 / §5.1).
//!
//! The owner-bound managers — chat sessions, chat hub, cron, and the
//! approval/clarification/elicitation/inbox interaction stack — must operate on
//! *one* user's `{userid}.db` and emit on *one* user's server→client channel, so
//! that no chat content, WS event, job or pending approval crosses between users.
//! They are built **lazily** on first use after the user's pool is unlocked, and
//! live exactly as long as that pool (§9: from first login until restart).
//!
//! `UserManager` stays the pool-lifecycle owner (§11 boundary). This factory sits
//! at the `Skald` layer, where the global *capability* managers (LLM, tools, MCP,
//! memory, providers) are visible, and stamps out the per-user instances against a
//! given pool, wiring their construction cycles and starting the per-user cron
//! loop. Capability managers are shared by reference; only owner-bound state is
//! per-user.
//!
//! Split of pools inside one context: session/history/jobs/hub use the **user
//! pool**; approval *rules* and `known_tools` are instance-wide registry data, so
//! `ApprovalManager` and `ToolDiscovery` read the **registry pool** (`system.db`)
//! while `ApprovalManager` still emits on the user's channel and keeps its own
//! pending map.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono_tz::Tz;
use sqlx::SqlitePool;
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

use core_api::approval::ApprovalApi;
use core_api::chat_hub::ChatHubApi;
use core_api::events::GlobalEvent;
use core_api::system_bus::SystemEventBus;
use core_api::user_channel::UserChannelHandle;

use crate::approval::ApprovalManager;
use crate::chat_event_bus::ChatEventBus;
use crate::chat_hub::ChatHub;
use crate::clarification::ClarificationManager;
use crate::compactor::ContextCompactor;
use crate::config::{CompactionConfig, CoreConfig, DatetimeConfig};
use crate::cron::TaskManager;
use crate::elicitation::ElicitationManager;
use crate::image_generate::ImageGeneratorManager;
use crate::inbox::Inbox;
use crate::llm::LlmManager;
use crate::mcp::McpManager;
use crate::memory::MemoryManager;
use crate::projects::tickets::ProjectTicketManager;
use crate::run_context::RunContextManager;
use crate::session::handler::{DEFAULT_MAX_PARALLEL_SUBAGENTS, DEFAULT_MAX_TOOL_ROUNDS};
use crate::session::manager::ChatSessionManager;
use crate::tool_discovery::ToolDiscovery;
use crate::tools::ToolRegistry;

use super::bundles::{Conversation, Integrations, Media, Models, Tools};
use super::runtime::Runtime;

/// One unlocked user's owner-bound runtime. Lifetime = the pool's lifetime.
pub struct UserContext {
    pub user_id:       String,
    pub pool:          Arc<SqlitePool>,
    pub event_bus:     Arc<ChatEventBus>,
    pub sessions:      Arc<ChatSessionManager>,
    pub chat_hub:      Arc<ChatHub>,
    pub cron:          Arc<TaskManager>,
    pub tickets:       Arc<ProjectTicketManager>,
    pub approval:      Arc<ApprovalManager>,
    pub clarification: Arc<ClarificationManager>,
    pub elicitation:   Arc<ElicitationManager>,
    pub inbox:         Inbox,
    /// Per-user server→client push channel. WS handlers subscribe here (via the
    /// hub) so a user's `ServerEvent`s never reach another user's socket.
    pub global_tx:     broadcast::Sender<GlobalEvent>,
}

/// Captures the global capability managers + resolved config once, and stamps out
/// a [`UserContext`] per unlocked pool.
pub(super) struct UserContextFactory {
    registry_pool:           Arc<SqlitePool>,
    llm_manager:             Arc<LlmManager>,
    tools:                   Arc<ToolRegistry>,
    mcp:                     Arc<McpManager>,
    memory_manager:          Arc<MemoryManager>,
    image_generator_manager: Arc<ImageGeneratorManager>,
    run_context_manager:     Arc<RunContextManager>,
    system_bus:              Arc<SystemEventBus>,
    supervisor:              Arc<super::supervisor::TaskSupervisor>,
    shutdown_token:          CancellationToken,
    max_history_messages:    usize,
    max_tool_rounds:         usize,
    max_parallel_subagents:  usize,
    max_tool_result_chars:   Option<usize>,
    datetime_config:         DatetimeConfig,
    compaction:              Option<CompactionConfig>,
    cron_tz:                 Option<Tz>,
}

impl UserContextFactory {
    pub(super) fn new(
        rt:           &Runtime,
        models:       &Models,
        media:        &Media,
        tools:        &Tools,
        integrations: &Integrations,
        conversation: &Conversation,
        config:       &CoreConfig,
    ) -> Self {
        let cron_tz = config.timezone.as_deref().and_then(|s| s.parse::<Tz>().ok());
        Self {
            registry_pool:           Arc::clone(&rt.db),
            llm_manager:             Arc::clone(&models.llm_manager),
            tools:                   Arc::clone(&tools.tools),
            mcp:                     Arc::clone(&integrations.mcp),
            memory_manager:          Arc::clone(&models.memory_manager),
            image_generator_manager: Arc::clone(&media.image_generator_manager),
            run_context_manager:     Arc::clone(&conversation.run_context_manager),
            system_bus:              Arc::clone(&rt.system_bus),
            supervisor:              Arc::clone(&rt.supervisor),
            shutdown_token:          rt.shutdown_token.clone(),
            max_history_messages:    config.llm.max_history_messages,
            max_tool_rounds:         config.llm.max_tool_rounds.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS),
            max_parallel_subagents:  config.llm.max_parallel_subagents.unwrap_or(DEFAULT_MAX_PARALLEL_SUBAGENTS),
            max_tool_result_chars:   config.llm.max_tool_result_chars,
            datetime_config:         DatetimeConfig { timezone: config.timezone.clone(), ..config.llm.datetime },
            compaction:              config.llm.compaction.clone(),
            cron_tz,
        }
    }

    async fn build(&self, user_id: &str, pool: SqlitePool) -> Result<Arc<UserContext>> {
        let pool = Arc::new(pool);
        let event_bus = Arc::new(ChatEventBus::new());
        let (global_tx, _) = broadcast::channel::<GlobalEvent>(512);

        // Interaction stack, per-user. Approval reads the shared registry rules but
        // emits on this user's channel and keeps its own pending map — no cross-user
        // collision on request_id / session_id. Rules are seeded once by the global
        // ApprovalManager at boot, so no re-seeding here.
        let approval = Arc::new(ApprovalManager::new(Arc::clone(&self.registry_pool), global_tx.clone()));
        let clarification = ClarificationManager::new(global_tx.clone());
        let elicitation = ElicitationManager::new(global_tx.clone());
        let inbox = Inbox::new(
            Arc::clone(&approval),
            Arc::clone(&clarification),
            Arc::clone(&elicitation),
            Arc::clone(&self.tools),
        );

        let compactor = self.compaction.as_ref().map(|cfg| {
            Arc::new(ContextCompactor::new(
                cfg.clone(),
                Arc::clone(&self.llm_manager),
                Arc::clone(&event_bus),
            ))
        });

        let manager = Arc::new(ChatSessionManager::new(
            Arc::clone(&pool),
            user_id.to_string(),
            Arc::clone(&self.llm_manager),
            self.max_history_messages,
            self.max_tool_rounds,
            self.max_parallel_subagents,
            self.max_tool_result_chars,
            self.datetime_config.clone(),
            Arc::clone(&self.tools),
            Arc::clone(&self.mcp),
            Arc::clone(&approval),
            Arc::clone(&clarification),
            Arc::clone(&event_bus),
            Arc::clone(&self.memory_manager),
            Arc::clone(&self.image_generator_manager),
            compactor,
            Arc::clone(&self.run_context_manager),
            // known_tools is registry data → discovery writes to the registry pool.
            Arc::new(ToolDiscovery::new(Arc::clone(&self.registry_pool))),
        ));

        let chat_hub = ChatHub::new(
            Arc::clone(&pool),
            Arc::clone(&manager),
            Arc::clone(&approval),
            global_tx.clone(),
            self.shutdown_token.clone(),
        );
        chat_hub.register("web").await;
        chat_hub.register("talk").await;

        let cron = TaskManager::new(Arc::clone(&pool), self.cron_tz, Arc::clone(&self.system_bus));
        cron.set_session(Arc::clone(&manager));
        cron.set_hub(Arc::clone(&chat_hub));
        cron.set_self_arc(Arc::clone(&cron));
        chat_hub.set_task_mgr(Arc::clone(&cron));

        // Per-user ticket manager — wired to the per-user TaskManager so
        // `start_ticket` spawns jobs in the user's own pool.
        let tickets = ProjectTicketManager::new(Arc::clone(&pool));
        tickets.set_task_manager(Arc::clone(&cron));

        // Per-user cron loop. `start()` observes the shutdown token, so it stops on
        // shutdown; adopting it lets the supervisor also join it. The name is leaked
        // to satisfy the `&'static str` label — bounded by the (small) user count.
        let name: &'static str = Box::leak(format!("cron:{user_id}").into_boxed_str());
        self.supervisor.adopt(name, Arc::clone(&cron).start(self.shutdown_token.clone()));

        // Per-user ticket-listener: reacts to JobCompleted events for this user's
        // tickets. All users' listeners receive the event (global system bus); only
        // the one that owns the ticket does the UPDATE — others no-op on 0 rows.
        let tname: &'static str = Box::leak(format!("tickets:{user_id}").into_boxed_str());
        self.supervisor.adopt_one(
            tname,
            Arc::clone(&tickets).start_listener(
                Arc::clone(&self.system_bus),
                self.shutdown_token.clone(),
            ),
        );

        Ok(Arc::new(UserContext {
            user_id: user_id.to_string(),
            pool,
            event_bus,
            sessions: manager,
            chat_hub,
            cron,
            tickets,
            approval,
            clarification,
            elicitation,
            inbox,
            global_tx,
        }))
    }
}

/// The live per-user contexts, keyed by user id, plus the factory that builds them.
/// A `tokio::Mutex` serialises the build so a context (and its cron loop) is created
/// at most once per user, even under concurrent first-use.
pub(super) struct UserContextRegistry {
    factory:  UserContextFactory,
    contexts: Mutex<HashMap<String, Arc<UserContext>>>,
}

impl UserContextRegistry {
    pub(super) fn new(factory: UserContextFactory) -> Self {
        Self { factory, contexts: Mutex::new(HashMap::new()) }
    }

    /// Returns the user's context, building it from `pool` on first use. Idempotent:
    /// once built, the same `Arc<UserContext>` is returned until restart.
    pub(super) async fn resolve(&self, user_id: &str, pool: SqlitePool) -> Result<Arc<UserContext>> {
        let mut guard = self.contexts.lock().await;
        if let Some(ctx) = guard.get(user_id) {
            return Ok(Arc::clone(ctx));
        }
        let ctx = self.factory.build(user_id, pool).await?;
        guard.insert(user_id.to_string(), Arc::clone(&ctx));
        Ok(ctx)
    }
}

// ── UserChannelHandle impl ────────────────────────────────────────────────────

/// Concrete [`UserChannelHandle`] wrapping a live [`UserContext`].
///
/// Constructed by [`Skald`](super::Skald) when resolving a user for a channel
/// plugin. The concrete type stays private — callers receive
/// `Arc<dyn UserChannelHandle>`.
pub(super) struct UserContextHandle {
    ctx: Arc<UserContext>,
}

impl UserContextHandle {
    pub(super) fn new(ctx: Arc<UserContext>) -> Self {
        Self { ctx }
    }
}

impl UserChannelHandle for UserContextHandle {
    fn user_id(&self) -> &str {
        &self.ctx.user_id
    }

    fn chat_hub(&self) -> Arc<dyn ChatHubApi> {
        Arc::clone(&self.ctx.chat_hub) as Arc<dyn ChatHubApi>
    }

    fn approval(&self) -> Arc<dyn ApprovalApi> {
        Arc::clone(&self.ctx.approval) as Arc<dyn ApprovalApi>
    }

    fn subscribe(&self) -> broadcast::Receiver<GlobalEvent> {
        self.ctx.global_tx.subscribe()
    }
}
