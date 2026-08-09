//! `UserLoopRuntime` — the loop stack of one user, built once.
//!
//! Everything that lives as long as the owner's pool lives here: the
//! `LoopManager` (event bus + live-loop registry), the history store, the
//! approval gate, the hooks, the agent catalog and the delegate tool. A turn
//! then contributes only what is genuinely its own — the agent's prompt, its
//! tool set, its model pin — through [`UserLoopRuntime::turn_params`].
//!
//! Why one per user and not one per turn (blueprint D12): the manager's job is
//! the *global* view — which conversations are running, `/stop`, recovery,
//! shutdown. A manager rebuilt for every message can answer none of those, and
//! rebuilding the graph per message also leaks it (the catalog ↔ delegate cycle
//! is broken by a `Weak`, but a per-turn graph would still pile up).

use std::sync::Arc;

use agent_loop::activation::ActivateToolsTool;
use agent_loop::delegate::DelegateTool;
use agent_loop::ids::ConversationId;
use agent_loop::manager::{LiveInput, LoopManager, TurnMeta, TurnParams};
use agent_loop::model::{ModelHint, ModelSelector};
use agent_loop::store::HistoryStore;
use agent_loop::tool::{Extensions, Tool as LoopTool, ToolSet};
use core_api::interface_tool::InterfaceTool;
use core_api::user_fs::SharedFs;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::approval::ApprovalManager;
use crate::clarification::ClarificationManager;
use crate::config::DatetimeConfig;
use crate::llm::LlmManager;
use crate::llm::logging::RequestLogTarget;
use crate::loop_adapters::activation::{SkaldActivationSource, SkaldToolActivator};
use crate::loop_adapters::async_task::CronExecutor;
use crate::loop_adapters::builtins::{
    ExecuteTaskAliasTool, LegacyInterfaceTool, SkaldAskUserTool, SkaldHumanChannel,
    UpdateScratchpadTool, WriteTodosTool,
};
use crate::loop_adapters::catalog::SkaldAgentCatalog;
use crate::loop_adapters::gate::ApprovalGate;
use crate::loop_adapters::history::SqliteHistory;
use crate::loop_adapters::hooks::{DtlReanchorHook, SkaldWritePreviewHook};
use crate::loop_adapters::live_input::PendingLiveInput;
use crate::loop_adapters::prefix_cache::PrefixCache;
use crate::loop_adapters::preview::PreviewContext;
use crate::loop_adapters::projection_cfg::skald_assembler;
use crate::loop_adapters::scope::TurnScope;
use crate::loop_adapters::selector::SkaldSelector;
use crate::loop_adapters::system::AgentSystemContext;
use crate::loop_adapters::toolset::{CallerMcp, CallerUserId, SkaldToolSet};
use crate::mcp::McpProvider;
use crate::session::handler::PendingUserInput;
use crate::session::handler::interface_tools::AgentRunConfig;
use crate::tool_discovery::ToolDiscovery;
use crate::tools::ToolRegistry;
use crate::tools::tool_names as tn;

/// Instance-wide loop limits (from `config.yml`).
#[derive(Clone)]
pub struct LoopConfig {
    pub max_rounds:            usize,
    pub max_parallel_calls:    usize,
    /// Sliding-window cap on projected history. `None` (the default) leaves
    /// history append-only — see `LlmConfig::max_history_messages`.
    pub max_history_messages:  Option<usize>,
    pub max_tool_result_chars: Option<usize>,
    /// Automatic compaction bounds the context instead of a message window.
    pub auto_compaction_enabled: bool,
    pub datetime:              DatetimeConfig,
    /// Allowlisted commands this user's container actually has, snapshotted at
    /// login — the prompt's discovery hint. See [`crate::container::commands`].
    pub sandbox_commands:      Arc<Vec<String>>,
    pub max_agent_depth:       u32,
}

/// Names handled natively; a legacy interface tool of the same name is dropped.
const NATIVE_NAMES: &[&str] = &[tn::ACTIVATE_TOOLS, tn::EXECUTE_TASK];

/// One user's loop stack.
pub struct UserLoopRuntime {
    manager:  Arc<LoopManager>,
    store:    Arc<dyn HistoryStore>,
    catalog:  Arc<SkaldAgentCatalog>,
    delegate: Arc<DelegateTool>,
    /// Backs `execute_task mode=async`; its `TaskManager` lands at wiring time.
    async_exec: Arc<CronExecutor>,
    // per-turn assembly material
    pool:           Arc<SqlitePool>,
    shared_pool:    Arc<SqlitePool>,
    user_id:        String,
    fs:             SharedFs,
    tools:          Arc<ToolRegistry>,
    mcp:            Arc<dyn McpProvider>,
    llm_manager:    Arc<LlmManager>,
    clarification:  Arc<ClarificationManager>,
    tool_discovery: Arc<ToolDiscovery>,
    config:         LoopConfig,
    /// The user's frozen system prefixes, shared with the agent catalog so a
    /// sub-agent's own prefix is cached alongside its parent's.
    prefix_cache:   Arc<PrefixCache>,
}

/// What a turn contributes on top of the runtime.
pub struct TurnInputs<'a> {
    pub scope:      Arc<TurnScope>,
    pub config:     &'a AgentRunConfig,
    /// Messages queued while the turn runs, drained at round boundaries.
    pub live_input: Option<Arc<dyn PendingUserInput>>,
}

impl UserLoopRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        pool:           Arc<SqlitePool>,
        shared_pool:    Arc<SqlitePool>,
        user_id:        String,
        fs:             SharedFs,
        tools:          Arc<ToolRegistry>,
        mcp:            Arc<dyn McpProvider>,
        llm_manager:    Arc<LlmManager>,
        approval:       Arc<ApprovalManager>,
        clarification:  Arc<ClarificationManager>,
        tool_discovery: Arc<ToolDiscovery>,
        config:         LoopConfig,
    ) -> anyhow::Result<Arc<Self>> {
        let store: Arc<dyn HistoryStore> = Arc::new(SqliteHistory::new(pool.clone()));

        let gate = ApprovalGate::new(
            approval.clone(),
            store.clone(),
            tools.clone(),
            pool.clone(),
            shared_pool.clone(),
            Some(fs.clone()),
        );

        let preview_hook = Arc::new(SkaldWritePreviewHook::new(PreviewContext {
            pool:        pool.clone(),
            shared_pool: shared_pool.clone(),
            fs:          Some(fs.clone()),
        }));

        // The default selector has no strength requirement; every turn overrides
        // it with the agent's own (D14). It carries the owner's log target, so a
        // call served by it (recovery, compaction) is still attributed.
        let default_selector: Arc<dyn ModelSelector> = Arc::new(
            SkaldSelector::new(llm_manager.clone(), None)
                .with_log(RequestLogTarget::user(user_id.clone(), pool.clone())),
        );

        let manager = Arc::new(
            LoopManager::builder()
                .models(default_selector)
                .store(store.clone())
                .gate_arc(Arc::new(gate))
                .hook(preview_hook)
                .hook(Arc::new(DtlReanchorHook::new(pool.clone())))
                .max_rounds(config.max_rounds)
                .max_parallel_calls(config.max_parallel_calls)
                .build()?,
        );

        // One per user, living as long as this runtime: a conversation's system
        // prefix must outlast its turns for the provider's cache to hold.
        let prefix_cache = Arc::new(PrefixCache::new());

        let catalog = Arc::new(SkaldAgentCatalog::new(
            pool.clone(),
            shared_pool.clone(),
            user_id.clone(),
            llm_manager.clone(),
            approval,
            clarification.clone(),
            mcp.clone(),
            tools.clone(),
            fs.clone(),
            config.clone(),
            prefix_cache.clone(),
        ));
        // `mode: "async"` runs as a durable cron job; the manager behind it is
        // set at wiring time (see `CronExecutor`).
        let async_exec = Arc::new(CronExecutor::new());
        let delegate = Arc::new(
            DelegateTool::new(
                manager.clone(),
                catalog.clone(),
                store.clone(),
                config.max_agent_depth,
            )
            .with_async(async_exec.clone()),
        );
        // The catalog hands `execute_subtask` to children; it holds this Weak.
        catalog.set_delegate(&delegate);

        Ok(Arc::new(Self {
            manager,
            store,
            catalog,
            delegate,
            async_exec,
            pool,
            shared_pool,
            user_id,
            fs,
            tools,
            mcp,
            llm_manager,
            clarification,
            tool_discovery,
            config,
            prefix_cache,
        }))
    }

    pub fn manager(&self) -> &Arc<LoopManager> {
        &self.manager
    }

    /// Hands the user's `TaskManager` to the async executor. Called once the
    /// cron side exists (it needs the session manager that owns this runtime).
    pub fn set_task_manager(&self, tasks: Arc<crate::cron::TaskManager>) {
        self.async_exec.set_task_manager(tasks);
    }

    pub fn store(&self) -> &Arc<dyn HistoryStore> {
        &self.store
    }

    /// Drops this user's frozen system prefixes — see [`PrefixCache::clear`].
    /// Called when the **skills index** they would carry has changed, which is
    /// the one case where waiting for the idle window would have the model deny
    /// that something exists.
    pub fn invalidate_prefixes(&self) {
        self.prefix_cache.clear();
    }

    /// Where this user's LLM traffic is logged: metadata in the registry
    /// (attributed to them), payloads in their own encrypted pool.
    pub fn log_target(&self) -> RequestLogTarget {
        RequestLogTarget::user(self.user_id.clone(), self.pool.clone())
    }

    /// The conversation id of a session — the store's encoding.
    pub fn conversation(session_id: i64) -> ConversationId {
        SqliteHistory::conversation(session_id)
    }

    /// Everything a turn needs, assembled from the run config and the scope.
    pub async fn turn_params(&self, inputs: TurnInputs<'_>) -> anyhow::Result<TurnParams> {
        let TurnInputs { scope, config, live_input } = inputs;
        let frame_agent = config.agent_id.clone();

        // The agent's own declarations. Loaded once here and used three times
        // below — for the sandbox hint, the tool set, and the selector's
        // strength floor.
        let meta = crate::agents::load_meta(&frame_agent).ok();

        // Whether this turn's model is shown `execute_cmd`, which is what gates
        // the sandbox command hint. Read from the same two things that decide the
        // tool set below and in that order: an agent declaring `allow_tools:
        // false` is shown nothing at all, and otherwise `base_tool_defs` has
        // already been through the security group's visibility filter
        // (`session/handler/config.rs`). Deriving it from the registry instead
        // would advertise a sandbox to exactly the agents that cannot reach it.
        let has_execute_cmd = meta.as_ref().is_none_or(|m| m.allow_tools)
            && config.base_tool_defs.iter().any(|d| {
                d["function"]["name"].as_str() == Some(crate::tools::tool_names::EXECUTE_CMD)
            });

        // ── System context ──
        let system = Arc::new(AgentSystemContext {
            agent_id:       frame_agent.clone(),
            extra_static:   config.extra_system.clone(),
            extra_dynamic:  config.extra_system_dynamic.clone(),
            tail_reminder:  config.tail_reminder.clone(),
            substitutions:  config.system_substitutions.clone(),
            pool:           self.pool.clone(),
            shared_pool:    self.shared_pool.clone(),
            user_id:        self.user_id.clone(),
            mcp:            self.mcp.clone(),
            fs:             self.fs.clone(),
            project_root:   scope.project_root.clone(),
            scratchpad_sid: scope.scratchpad_sid,
            datetime:       self.config.datetime.clone(),
            sandbox_commands: self.config.sandbox_commands.clone(),
            has_execute_cmd,
            prefix_cache:   self.prefix_cache.clone(),
        });

        // ── Tool set: the native tools, then the surface's legacy ones ──
        //
        // Unless the agent declares it gets none. An empty set is not the same as
        // a restrictive permission group: a group decides whether a call is
        // allowed, this decides whether the model is shown anything to call. For
        // an agent that reads material and answers in prose — a review, a
        // summariser — that is the difference between gating an action and there
        // being no action available.
        let tools = match meta.as_ref() {
            Some(m) if !m.allow_tools => Arc::new(agent_loop::tool::ToolRegistry::new()) as Arc<dyn ToolSet>,
            _                         => self.build_toolset(&scope, config),
        };

        // ── Assembler: the shared projection, scoped to this session's DTL ──
        let assembler = Arc::new(skald_assembler(
            Arc::new(SkaldActivationSource::new(
                self.pool.clone(),
                self.mcp.clone(),
                scope.config_defs.clone(),
                scope.session_id,
                None,
            )),
            Some(self.fs.load()),
            self.config.max_history_messages,
            self.config.auto_compaction_enabled,
            self.config.max_tool_result_chars,
        ));

        // ── Extensions: the tool bridge's context + the turn's own scope ──
        let mut extensions = Extensions::new();
        extensions.insert(self.pool.clone());
        extensions.insert(self.fs.load());
        extensions.insert(Arc::new(CallerUserId(self.user_id.clone())));
        extensions.insert(Arc::new(CallerMcp(Arc::new(
            crate::mcp::McpDirectoryHandle(self.mcp.clone()),
        ))));
        extensions.insert(scope.clone());

        // ── Selector: this agent's strength (D14) + the owner's request log ──
        let strength = meta.and_then(|m| m.strength);
        let selector: Arc<dyn ModelSelector> = Arc::new(
            SkaldSelector::new(self.llm_manager.clone(), strength).with_log(self.log_target()),
        );

        // The session's root frame; the store reuses the provisioned row.
        let frame = self
            .store
            .open_frame(
                &Self::conversation(scope.session_id),
                None,
                agent_loop::store::FrameSpec::root(&frame_agent),
            )
            .await?;

        Ok(TurnParams {
            frame,
            agent: frame_agent,
            system,
            tools,
            model_hint: ModelHint::name(config.client_name.clone()),
            selector: Some(selector),
            live_input: live_input
                .map(|p| Arc::new(PendingLiveInput::new(p)) as Arc<dyn LiveInput>),
            extensions,
            meta: TurnMeta {
                synthetic:     false,
                interactive:   scope.is_interactive,
                context_label: scope.context_label.read().ok().and_then(|g| g.clone()),
                user_message:  None,
            },
            assembler: Some(assembler),
        })
    }

    /// The root agent's tool set: natives (activation, delegation, clarification,
    /// scratchpad, todos) plus the surface's own interface tools.
    fn build_toolset(&self, scope: &Arc<TurnScope>, config: &AgentRunConfig) -> Arc<dyn ToolSet> {
        let mut native: Vec<Arc<dyn LoopTool>> = Vec::new();

        // activate_tools, sharing the turn's grant set so the next round sees
        // whatever this round activated.
        native.push(Arc::new(
            ActivateToolsTool::new(Arc::new(SkaldToolActivator::new(
                self.pool.clone(),
                self.shared_pool.clone(),
                self.user_id.clone(),
                self.mcp.clone(),
                scope.config_defs.clone(),
                scope.grants.clone(),
                scope.session_id,
                None,
            )))
            .with_definition(crate::session::handler::config::activate_tools_tool_def()),
        ));

        // execute_task: sync/async → the delegate; cron → the scheduling handler.
        {
            let injected = config
                .interface_tools
                .iter()
                .find(|it| it.definition["function"]["name"].as_str() == Some(tn::EXECUTE_TASK))
                .cloned();
            let (def, handler) = match injected {
                Some(it) => (it.definition.clone(), Some(it.handler.clone())),
                None => (legacy_execute_task_def(), None),
            };
            native.push(Arc::new(ExecuteTaskAliasTool::new(
                self.delegate.as_ref().clone().with_name(tn::EXECUTE_TASK),
                def,
                handler,
            )));
        }

        native.push(Arc::new(SkaldAskUserTool::new(
            Arc::new(SkaldHumanChannel::new(
                self.clarification.clone(),
                scope.session_id,
                &scope.agent_id,
                &scope.source,
                scope.is_interactive,
                scope.context_label.clone(),
            )),
            self.store.clone(),
        )));
        native.push(Arc::new(UpdateScratchpadTool::new(
            self.pool.clone(),
            scope.scratchpad_sid,
        )));
        native.push(Arc::new(WriteTodosTool));

        let legacy: Vec<InterfaceTool> = config
            .interface_tools
            .iter()
            .filter(|it| {
                let name = it.definition["function"]["name"].as_str().unwrap_or("");
                !NATIVE_NAMES.contains(&name)
            })
            .cloned()
            .collect();
        for it in &legacy {
            native.push(Arc::new(LegacyInterfaceTool::new(it.clone())));
        }

        Arc::new(
            SkaldToolSet::new(
                scope.base_defs.as_ref().clone(),
                scope.config_defs.clone(),
                self.mcp.clone(),
                scope.grants.clone(),
                scope.memory_tools.as_ref().clone(),
                scope.image_tools.as_ref().clone(),
                legacy,
                self.tools.all_tools(),
            )
            .with_discovery(self.tool_discovery.clone())
            .with_native_all(native),
        )
    }

    /// The catalog, for callers that list dispatchable agents.
    pub fn catalog(&self) -> &Arc<SkaldAgentCatalog> {
        &self.catalog
    }
}

/// Fallback definition for `execute_task` when no interface handler was injected
/// (non-interactive sessions): mirrors the injected one.
fn legacy_execute_task_def() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": tn::EXECUTE_TASK,
            "description": "Execute a task with a sub-agent. mode=sync waits for the result; \
                            mode=async schedules it in the background.",
            "parameters": {
                "type": "object",
                "properties": {
                    "agent_id":    { "type": "string" },
                    "prompt":      { "type": "string" },
                    "title":       { "type": "string" },
                    "description": { "type": "string" },
                    "mode":        { "type": "string", "enum": ["sync", "async"] },
                    "client":      { "type": "string" }
                },
                "required": ["agent_id", "prompt"]
            }
        }
    })
}
