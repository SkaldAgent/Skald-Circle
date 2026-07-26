//! `SkaldAgentCatalog` — the crate's `AgentCatalog` over `agents/*`
//! (port of `build_sub_agent_config`, blueprint §10): builds the child's
//! profile — its own prompt (never the parent's, B3), derived tool set
//! (root-only strip + sub-agent augmentation + approval visibility), own
//! strength selector (D14), own DTL-scoped assembler and activator.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use agent_loop::context::ContextAssembler;
use agent_loop::delegate::{AgentCatalog, AgentKind, AgentProfile, AgentSummary, DelegateTool, ToolSelection};
use agent_loop::ids::FrameId;
use agent_loop::model::ModelHint;
use agent_loop::tool::Tool as LoopTool;
use agent_loop::activation::ActivateToolsTool;
use sqlx::SqlitePool;

use crate::approval::ApprovalManager;
use crate::clarification::ClarificationManager;
use crate::config::DatetimeConfig;
use crate::llm::LlmManager;
use crate::loop_adapters::activation::SkaldToolActivator;
use crate::loop_adapters::assembler::SkaldAssembler;
use crate::loop_adapters::builtins::{SkaldAskUserTool, SkaldHumanChannel};
use crate::loop_adapters::history::SqliteHistory;
use crate::loop_adapters::selector::SkaldSelector;
use crate::loop_adapters::system::AgentSystemContext;
use crate::loop_adapters::toolset::SkaldToolSet;
use crate::mcp::McpProvider;
use crate::tools::ToolRegistry;
use crate::tools::tool_names as tn;

/// Everything the catalog needs from the parent turn, captured at wiring time.
pub struct SkaldAgentCatalog {
    pool:          Arc<SqlitePool>,
    shared_pool:   Arc<SqlitePool>,
    user_id:       String,
    session_id:    i64,
    source:        String,
    is_interactive: bool,
    context_label: Arc<RwLock<Option<String>>>,
    llm_manager:   Arc<LlmManager>,
    approval:      Arc<ApprovalManager>,
    clarification: Arc<ClarificationManager>,
    mcp:           Arc<dyn McpProvider>,
    registry:      Arc<ToolRegistry>,
    /// Parent turn's derived def lists (the child's base derives from these).
    base_defs:     Vec<serde_json::Value>,
    config_defs:   Arc<Vec<serde_json::Value>>,
    memory_tools:  Vec<Arc<dyn crate::tools::Tool>>,
    image_tools:   Vec<Arc<dyn crate::tools::Tool>>,
    core_tools:    Vec<Arc<dyn crate::tools::Tool>>,
    root_only:     Vec<String>,
    /// The delegate tool, injected post-construction (catalog ↔ delegate cycle).
    delegate:      RwLock<Option<Arc<DelegateTool>>>,
    /// Per-turn assembler knobs shared with children.
    datetime_config:       DatetimeConfig,
    max_history_messages:  usize,
    max_tool_result_chars: Option<usize>,
    compactor_enabled:     bool,
    fs:                    Option<Arc<core_api::user_fs::UserFs>>,
    project_root:          Option<String>,
}

impl SkaldAgentCatalog {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool:          Arc<SqlitePool>,
        shared_pool:   Arc<SqlitePool>,
        user_id:       String,
        session_id:    i64,
        source:        String,
        is_interactive: bool,
        context_label: Arc<RwLock<Option<String>>>,
        llm_manager:   Arc<LlmManager>,
        approval:      Arc<ApprovalManager>,
        clarification: Arc<ClarificationManager>,
        mcp:           Arc<dyn McpProvider>,
        registry:      Arc<ToolRegistry>,
        base_defs:     Vec<serde_json::Value>,
        config_defs:   Arc<Vec<serde_json::Value>>,
        memory_tools:  Vec<Arc<dyn crate::tools::Tool>>,
        image_tools:   Vec<Arc<dyn crate::tools::Tool>>,
        root_only:     Vec<String>,
        datetime_config:       DatetimeConfig,
        max_history_messages:  usize,
        max_tool_result_chars: Option<usize>,
        compactor_enabled:     bool,
        fs:                    Option<Arc<core_api::user_fs::UserFs>>,
        project_root:          Option<String>,
    ) -> Self {
        let core_tools = registry.all_tools();
        Self {
            pool,
            shared_pool,
            user_id,
            session_id,
            source,
            is_interactive,
            context_label,
            llm_manager,
            approval,
            clarification,
            mcp,
            registry,
            base_defs,
            config_defs,
            memory_tools,
            image_tools,
            core_tools,
            root_only,
            delegate: RwLock::new(None),
            datetime_config,
            max_history_messages,
            max_tool_result_chars,
            compactor_enabled,
            fs,
            project_root,
        }
    }

    /// Post-construction wiring of the delegate (the catalog ↔ delegate cycle).
    pub fn set_delegate(&self, delegate: DelegateTool) {
        *self.delegate.write().unwrap() = Some(Arc::new(delegate));
    }
}

#[agent_loop::async_trait]
impl AgentCatalog for SkaldAgentCatalog {
    async fn get(&self, id: &str, child_frame: FrameId) -> agent_loop::Result<AgentProfile> {
        // Only `task` agents are dispatchable (rejects chat/system/unknown).
        let meta = crate::agents::load_task_meta(id)
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        // The child's own strength drives its selector (D14) — never the
        // parent's resolved client.
        let selector = Arc::new(SkaldSelector::new(self.llm_manager.clone(), meta.strength));
        let model = meta.client.as_deref().map(ModelHint::name);

        // The child's system context: its own prompt, no per-turn extras.
        let context = Arc::new(AgentSystemContext {
            agent_id:      id.to_string(),
            extra_static:  None,
            extra_dynamic: None,
            tail_reminder: None,
            substitutions: Default::default(),
            pool:          self.pool.clone(),
            shared_pool:   self.shared_pool.clone(),
            user_id:       self.user_id.clone(),
            mcp:           self.mcp.clone(),
            project_root:  self.project_root.clone(),
        });

        // The child's def list: parent's base minus root-only minus the
        // re-derived augmentations (added back natively below), plus
        // sub-agents-only tools, through the approval visibility filter.
        let mut child_defs: Vec<serde_json::Value> = self
            .base_defs
            .iter()
            .filter(|d| {
                let name = d["function"]["name"].as_str().unwrap_or("");
                !self.root_only.iter().any(|n| n == name)
                    && name != tn::ASK_USER_CLARIFICATION
                    && name != tn::EXECUTE_SUBTASK
                    && name != tn::EXECUTE_TASK
            })
            .cloned()
            .collect();
        child_defs.extend(self.registry.openai_definitions_sub_agents_only());
        {
            let group_rules = crate::db::approval_rules::list_for_group(&self.shared_pool, None)
                .await
                .unwrap_or_default();
            child_defs.retain(|def| {
                let name = def["function"]["name"].as_str().unwrap_or("");
                self.approval.is_tool_visible(&group_rules, name)
            });
        }

        // Native child tools: clarification, sub-delegation (depth permitting),
        // and the frame-scoped activate_tools with a FRESH grant set.
        let child_grants: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(
            crate::db::activated_tools::list_refs_stack(&self.pool, child_frame.get())
                .await
                .unwrap_or_default()
                .into_iter()
                .collect(),
        ));

        let mut native: Vec<Arc<dyn LoopTool>> = Vec::new();
        {
            let channel = Arc::new(SkaldHumanChannel::new(
                self.clarification.clone(),
                self.session_id,
                id,
                &self.source,
                self.is_interactive,
                self.context_label.clone(),
            ));
            native.push(Arc::new(SkaldAskUserTool::new(
                channel,
                Arc::new(SqliteHistory::new(self.pool.clone())),
            )));
        }
        // `execute_subtask` only while the child can still recurse.
        let delegate = self.delegate.read().unwrap().clone();
        if let Some(d) = delegate {
            native.push(Arc::new(d.as_ref().clone().with_name(tn::EXECUTE_SUBTASK)));
        }
        native.push(Arc::new(ActivateToolsTool::new(Arc::new(SkaldToolActivator::new(
            self.pool.clone(),
            self.mcp.clone(),
            child_grants.clone(),
            self.session_id,
            Some(child_frame.get()),
        )))));

        let toolset: Arc<dyn agent_loop::tool::ToolSet> = Arc::new(
            SkaldToolSet::new(
                child_defs,
                self.config_defs.clone(),
                self.mcp.clone(),
                child_grants,
                self.memory_tools.clone(),
                self.image_tools.clone(),
                Vec::new(),
                self.core_tools.clone(),
            )
            .with_native_all(native),
        );

        let assembler: Arc<dyn ContextAssembler> = Arc::new(SkaldAssembler {
            pool:                  self.pool.clone(),
            scratchpad_sid:        self.session_id,
            datetime_config:       self.datetime_config.clone(),
            max_history_messages:  self.max_history_messages,
            max_tool_result_chars: self.max_tool_result_chars,
            compactor_enabled:     self.compactor_enabled,
            fs:                    self.fs.clone(),
            activation: Some(crate::loop_adapters::activation::SkaldActivationSource::new(
                self.pool.clone(),
                self.mcp.clone(),
                self.config_defs.clone(),
                self.session_id,
                Some(child_frame.get()),
            )),
        });

        Ok(AgentProfile {
            id: id.to_string(),
            kind: AgentKind::Task,
            context,
            tools: ToolSelection::inherit(),
            model,
            selector: Some(selector),
            assembler: Some(assembler),
            toolset: Some(toolset),
        })
    }

    async fn list(&self, kind: AgentKind) -> Vec<AgentSummary> {
        if kind != AgentKind::Task {
            return Vec::new();
        }
        crate::agents::discover()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| matches!(a.agent_type, crate::agents::AgentType::Task))
            .map(|a| AgentSummary { id: a.id, kind, description: a.description })
            .collect()
    }

    async fn on_child_closed(&self, frame: FrameId) {
        // Stack-scoped activations are ephemeral — deleted on frame exit.
        if let Err(e) = crate::db::activated_tools::delete_for_stack(&self.pool, frame.get()).await {
            tracing::warn!(frame = %frame, error = %e, "catalog: failed to delete stack activations");
        }
    }
}
