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
use core_api::inbox::InboxApi;
use core_api::system_bus::SystemEventBus;
use core_api::user_channel::UserChannelHandle;
use core_api::user_fs::SharedFs;

use crate::approval::ApprovalManager;
use crate::chat_event_bus::ChatEventBus;
use crate::chat_hub::ChatHub;
use crate::clarification::ClarificationManager;
use crate::compactor::ContextCompactor;
use crate::config::{CompactionConfig, CoreConfig, DatetimeConfig};
use crate::container::ContainerManager;
use crate::cron::TaskManager;
use crate::elicitation::ElicitationManager;
use crate::image_generate::ImageGeneratorManager;
use crate::inbox::Inbox;
use crate::llm::LlmManager;
use crate::mcp::{McpManager, McpProvider, UserMcpView};
use crate::memory::MemoryManager;
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
    /// The owner's filesystem view (home + shared folders + container, §6),
    /// threaded into every `ToolContext` this user's sessions produce. A shared
    /// swappable cell so a shared-folder membership change is applied in place
    /// (§6 remount) rather than requiring a fresh login — see [`SharedFs`].
    pub fs:            SharedFs,
    pub event_bus:     Arc<ChatEventBus>,
    pub sessions:      Arc<ChatSessionManager>,
    pub chat_hub:      Arc<ChatHub>,
    pub cron:          Arc<TaskManager>,
    pub approval:      Arc<ApprovalManager>,
    pub clarification: Arc<ClarificationManager>,
    pub elicitation:   Arc<ElicitationManager>,
    pub inbox:         Inbox,
    /// This user's own MCP runtime (blueprint §7/§9): connectors that run inside
    /// their container, started at first login and living until restart. Held
    /// here so its lifetime equals the pool's; its `docker exec -i` children die
    /// via `kill_on_drop` when the context is dropped at shutdown.
    pub user_mcp:      Arc<McpManager>,
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
    /// The GLOBAL MCP runtime (host, shared). Unioned per-user with the per-user
    /// runtime built at login (`UserMcpView`).
    mcp:                     Arc<McpManager>,
    /// Container lifecycle — used to ensure a user's container is up before their
    /// per-user (container-hosted) MCP connectors start.
    container:               ContainerManager,
    memory_manager:          Arc<MemoryManager>,
    image_generator_manager: Arc<ImageGeneratorManager>,
    run_context_manager:     Arc<RunContextManager>,
    system_bus:              Arc<SystemEventBus>,
    /// The single shared chat-turn bus. Every per-user `UserContext` publishes its
    /// completed turns here (tagged with `user_id`) so a global consumer — the
    /// Honcho memory sink — can observe every user's turns from one subscription
    /// (`Skald::subscribe_chat_events`) and demux by `ChatEvent.user_id`.
    event_bus:               Arc<ChatEventBus>,
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
        container:    &ContainerManager,
        config:       &CoreConfig,
    ) -> Self {
        let cron_tz = config.timezone.as_deref().and_then(|s| s.parse::<Tz>().ok());
        Self {
            registry_pool:           Arc::clone(&rt.db),
            llm_manager:             Arc::clone(&models.llm_manager),
            tools:                   Arc::clone(&tools.tools),
            mcp:                     Arc::clone(&integrations.mcp),
            container:               container.clone(),
            memory_manager:          Arc::clone(&models.memory_manager),
            image_generator_manager: Arc::clone(&media.image_generator_manager),
            run_context_manager:     Arc::clone(&conversation.run_context_manager),
            system_bus:              Arc::clone(&rt.system_bus),
            event_bus:               Arc::clone(&rt.event_bus),
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
        // The owner's filesystem view: private home + shared folders + container.
        // A shared swappable cell — a shared-folder membership change is applied in
        // place while the user is live (§6 remount), not deferred to next login.
        let fs = SharedFs::new(crate::container::build_user_fs(&self.registry_pool, user_id).await?);
        // Shared, not per-user: publish this user's turns onto the one global bus so
        // the Honcho sink sees every user from a single subscription (demux by
        // `ChatEvent.user_id`). See the field doc on `UserContextFactory::event_bus`.
        let event_bus = Arc::clone(&self.event_bus);
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

        // Per-user MCP runtime (blueprint §7/§9): the connectors this user has
        // activated, run INSIDE their container. Started here on first login and
        // living until restart — its `docker exec -i` children die via
        // `kill_on_drop` when this context (holding `user_mcp`) is dropped at
        // shutdown. Ensure the container is up first (idempotent: boot
        // reconciliation and user-create already do this; the belt-and-braces call
        // recovers a container stopped since). Non-fatal — a container hiccup
        // degrades MCP/exec but must not block login.
        if let Err(e) = self.container.ensure(user_id).await {
            tracing::warn!(user = %user_id, error = %e, "failed to ensure container before per-user MCP start");
        }
        let user_mcp = Arc::new(McpManager::new(
            Arc::clone(&pool),
            self.shutdown_token.clone(),
            "data",
        ));
        // NOTE: per-user MCP elicitation (interactive connector login, §15) is
        // deferred — api-key connectors don't need it. Wire the user's
        // ElicitationBridge here when interactive auth lands.
        {
            let um        = Arc::clone(&user_mcp);
            let upool     = Arc::clone(&pool);
            let registry  = Arc::clone(&self.registry_pool);
            let container = crate::container::container_name(user_id);
            let uid       = user_id.to_string();
            let mname: &'static str = Box::leak(format!("mcp:{user_id}").into_boxed_str());
            self.supervisor.adopt_one(mname, tokio::spawn(async move {
                match crate::db::mcp_user_servers::all_startable(&upool).await {
                    Ok(rows) => {
                        // Access filter (deny-by-default): a catalog-derived connector
                        // starts only while the admin still grants this user access to
                        // it. Self-registered remotes (no `catalog_name`) are the user's
                        // own to run. A revoked connector therefore stays dormant from
                        // the next login on, even though its activation row persists in
                        // the user's database (which the admin cannot reach while locked).
                        let mut startable = Vec::with_capacity(rows.len());
                        for r in rows {
                            let allowed = match &r.catalog_name {
                                Some(cat) => crate::db::mcp_catalog_access::has_access(&registry, cat, &uid)
                                    .await
                                    .unwrap_or(false),
                                None => true,
                            };
                            if allowed {
                                startable.push(r);
                            } else {
                                tracing::info!(user = %uid, connector = %r.name, "per-user MCP: not starting — catalog access not granted");
                            }
                        }
                        let mut specs = Vec::with_capacity(startable.len());
                        for r in &startable {
                            // Reconcile files + node/python deps in the container
                            // before starting (covers a fresh container and any
                            // connector update — see `prepare_local_connector`).
                            crate::mcp::prepare_local_connector(&registry, &uid, &container, r).await;
                            // OAuth connectors resolve their stored refresh token into
                            // the env-delivered credential here (§15).
                            specs.push(crate::mcp::user_row_spec_resolved(r, &container, &registry).await);
                        }
                        um.connect_all(specs, false).await;
                    }
                    Err(e) => tracing::warn!(error = %e, "per-user MCP init: failed to read mcp_user_servers"),
                }
            }));
        }

        // The MCP view this user's sessions see: the access-filtered global runtime
        // unioned with their per-user runtime (§7). `accessible_global` is a
        // snapshot of `mcp_global_access`, captured at build time like fs membership.
        let accessible_global: std::collections::HashSet<String> =
            crate::db::mcp_global_access::server_names_for_user(&self.registry_pool, user_id)
                .await
                .unwrap_or_default()
                .into_iter()
                .collect();
        let mcp_view: Arc<dyn McpProvider> = Arc::new(UserMcpView {
            global: Arc::clone(&self.mcp),
            user:   Arc::clone(&user_mcp),
            accessible_global,
        });

        let manager = Arc::new(ChatSessionManager::new(
            Arc::clone(&pool),
            Arc::clone(&self.registry_pool), // shared pool = system.db, for shared-memory injection
            user_id.to_string(),
            fs.clone(),
            Arc::clone(&self.llm_manager),
            self.max_history_messages,
            self.max_tool_rounds,
            self.max_parallel_subagents,
            self.max_tool_result_chars,
            self.datetime_config.clone(),
            Arc::clone(&self.tools),
            mcp_view,
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

        // Per-user cron loop. `start()` observes the shutdown token, so it stops on
        // shutdown; adopting it lets the supervisor also join it. The name is leaked
        // to satisfy the `&'static str` label — bounded by the (small) user count.
        let name: &'static str = Box::leak(format!("cron:{user_id}").into_boxed_str());
        self.supervisor.adopt(name, Arc::clone(&cron).start(self.shutdown_token.clone()));

        Ok(Arc::new(UserContext {
            user_id: user_id.to_string(),
            pool,
            fs,
            event_bus,
            sessions: manager,
            chat_hub,
            cron,
            approval,
            clarification,
            elicitation,
            inbox,
            user_mcp,
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

    /// The user's context IF already built (live), **without** building one. A user
    /// who has not logged in has no live snapshot to refresh (blueprint §6 remount):
    /// their next login builds a fresh context that already reflects the change.
    pub(super) async fn peek(&self, user_id: &str) -> Option<Arc<UserContext>> {
        self.contexts.lock().await.get(user_id).cloned()
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

    fn inbox(&self) -> Arc<dyn InboxApi> {
        // `Inbox` is a cheap facade over the interaction-stack `Arc`s (Clone);
        // the clone shares the same pending state as the context's own inbox.
        Arc::new(self.ctx.inbox.clone()) as Arc<dyn InboxApi>
    }

    fn subscribe(&self) -> broadcast::Receiver<GlobalEvent> {
        self.ctx.global_tx.subscribe()
    }
}
