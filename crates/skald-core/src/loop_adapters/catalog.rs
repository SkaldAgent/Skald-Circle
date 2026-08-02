//! `SkaldAgentCatalog` — the crate's `AgentCatalog` over `agents/*`
//! (port of `build_sub_agent_config`, blueprint §10): builds the child's
//! profile — its own prompt (never the parent's, B3), derived tool set
//! (root-only strip + sub-agent augmentation + approval visibility), own
//! strength selector (D14), own DTL-scoped assembler and activator.
//!
//! Built **once per user**: everything about the delegating turn comes from the
//! call's [`TurnScope`], never captured here.

use std::collections::HashSet;
use std::sync::{Arc, RwLock, Weak};

use agent_loop::context::ContextAssembler;
use agent_loop::delegate::{
    AgentCatalog, AgentKind, AgentProfile, AgentSummary, DelegateTool, ToolSelection,
};
use agent_loop::ids::FrameId;
use agent_loop::model::ModelHint;
use agent_loop::tool::{Tool as LoopTool, ToolCtx};
use agent_loop::activation::ActivateToolsTool;
use core_api::user_fs::SharedFs;
use sqlx::SqlitePool;

use crate::approval::ApprovalManager;
use crate::clarification::ClarificationManager;
use crate::llm::LlmManager;
use crate::llm::logging::RequestLogTarget;
use crate::loop_adapters::activation::SkaldToolActivator;
use crate::loop_adapters::builtins::{SkaldAskUserTool, SkaldHumanChannel};
use crate::loop_adapters::history::SqliteHistory;
use crate::loop_adapters::runtime::LoopConfig;
use crate::loop_adapters::scope::TurnScope;
use crate::loop_adapters::selector::SkaldSelector;
use crate::loop_adapters::system::AgentSystemContext;
use crate::loop_adapters::toolset::SkaldToolSet;
use crate::mcp::McpProvider;
use crate::tools::ToolRegistry;
use crate::tools::tool_names as tn;

/// The catalog's own dependencies — all of them user-scoped.
pub struct SkaldAgentCatalog {
    pool:          Arc<SqlitePool>,
    shared_pool:   Arc<SqlitePool>,
    user_id:       String,
    llm_manager:   Arc<LlmManager>,
    approval:      Arc<ApprovalManager>,
    clarification: Arc<ClarificationManager>,
    mcp:           Arc<dyn McpProvider>,
    registry:      Arc<ToolRegistry>,
    core_tools:    Vec<Arc<dyn crate::tools::Tool>>,
    /// The swappable fs cell, so a §6 remount reaches sub-agents too.
    fs:            SharedFs,
    config:        LoopConfig,
    /// The delegate tool, injected post-construction. **Weak** on purpose: the
    /// delegate holds the catalog, so an `Arc` here would be a cycle that never
    /// frees (and this graph lives as long as the user).
    delegate:      RwLock<Weak<DelegateTool>>,
}

impl SkaldAgentCatalog {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool:          Arc<SqlitePool>,
        shared_pool:   Arc<SqlitePool>,
        user_id:       String,
        llm_manager:   Arc<LlmManager>,
        approval:      Arc<ApprovalManager>,
        clarification: Arc<ClarificationManager>,
        mcp:           Arc<dyn McpProvider>,
        registry:      Arc<ToolRegistry>,
        fs:            SharedFs,
        config:        LoopConfig,
    ) -> Self {
        let core_tools = registry.all_tools();
        Self {
            pool,
            shared_pool,
            user_id,
            llm_manager,
            approval,
            clarification,
            mcp,
            registry,
            core_tools,
            fs,
            config,
            delegate: RwLock::new(Weak::new()),
        }
    }

    /// Post-construction wiring of the delegate (catalog ↔ delegate cycle,
    /// broken by the `Weak` above).
    pub fn set_delegate(&self, delegate: &Arc<DelegateTool>) {
        *self.delegate.write().unwrap() = Arc::downgrade(delegate);
    }
}

#[agent_loop::async_trait]
impl AgentCatalog for SkaldAgentCatalog {
    async fn get(
        &self,
        id:          &str,
        child_frame: FrameId,
        ctx:         &ToolCtx,
    ) -> agent_loop::Result<AgentProfile> {
        let scope = TurnScope::from(&ctx.extensions)
            .ok_or_else(|| anyhow::anyhow!("delegate: the turn published no scope"))?;

        // Only `task` agents are dispatchable (rejects chat/system/unknown).
        let meta = crate::agents::load_task_meta(id).map_err(|e| anyhow::anyhow!("{e}"))?;

        // The child's own strength drives its selector (D14) — never the
        // parent's resolved client. Its traffic is logged under the same owner
        // (the child's frame id already distinguishes it in the log).
        let selector = Arc::new(
            SkaldSelector::new(self.llm_manager.clone(), meta.strength)
                .with_log(RequestLogTarget::user(self.user_id.clone(), self.pool.clone())),
        );
        let model = meta.client.as_deref().map(ModelHint::name);

        // The child's system context: its own prompt, no per-turn extras.
        let context = Arc::new(AgentSystemContext {
            agent_id:       id.to_string(),
            extra_static:   None,
            extra_dynamic:  None,
            tail_reminder:  None,
            substitutions:  Default::default(),
            pool:           self.pool.clone(),
            shared_pool:    self.shared_pool.clone(),
            user_id:        self.user_id.clone(),
            mcp:            self.mcp.clone(),
            project_root:   scope.project_root.clone(),
            // The scratchpad is the session's blackboard: a sub-agent reads and
            // writes the SAME one as its parent.
            scratchpad_sid: scope.scratchpad_sid,
            datetime:       self.config.datetime.clone(),
        });

        // The child's def list: parent's base minus root-only minus the
        // re-derived augmentations (added back natively below), plus
        // sub-agents-only tools, through the approval visibility filter.
        let mut child_defs: Vec<serde_json::Value> = scope
            .base_defs
            .iter()
            .filter(|d| {
                let name = d["function"]["name"].as_str().unwrap_or("");
                !scope.root_only.iter().any(|n| n == name)
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
        // and the frame-scoped activate_tools with a FRESH grant set — a child
        // never inherits the parent's activations.
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
                scope.session_id,
                id,
                &scope.source,
                scope.is_interactive,
                scope.context_label.clone(),
            ));
            native.push(Arc::new(SkaldAskUserTool::new(
                channel,
                Arc::new(SqliteHistory::new(self.pool.clone())),
            )));
        }
        // `execute_subtask` only while the child can still recurse. A dead Weak
        // means the runtime is shutting down: the child simply cannot delegate.
        if let Some(d) = self.delegate.read().unwrap().upgrade() {
            // Legacy name AND legacy schema (D11): a sub-agent sees the same
            // definition it has always seen, not the crate's generic one.
            native.push(Arc::new(
                d.as_ref()
                    .clone()
                    .with_name(tn::EXECUTE_SUBTASK)
                    .with_definition(crate::session::handler::execute_subtask_tool_def()),
            ));
        }
        native.push(Arc::new(ActivateToolsTool::new(Arc::new(SkaldToolActivator::new(
            self.pool.clone(),
            self.shared_pool.clone(),
            self.user_id.clone(),
            self.mcp.clone(),
            scope.config_defs.clone(),
            child_grants.clone(),
            scope.session_id,
            Some(child_frame.get()),
        )))));

        let toolset: Arc<dyn agent_loop::tool::ToolSet> = Arc::new(
            SkaldToolSet::new(
                child_defs,
                scope.config_defs.clone(),
                self.mcp.clone(),
                child_grants,
                scope.memory_tools.as_ref().clone(),
                scope.image_tools.as_ref().clone(),
                Vec::new(),
                self.core_tools.clone(),
            )
            .with_native_all(native),
        );

        let assembler: Arc<dyn ContextAssembler> = Arc::new(
            crate::loop_adapters::projection_cfg::skald_assembler(
                Arc::new(crate::loop_adapters::activation::SkaldActivationSource::new(
                    self.pool.clone(),
                    self.mcp.clone(),
                    scope.config_defs.clone(),
                    scope.session_id,
                    Some(child_frame.get()),
                )),
                Some(self.fs.load()),
                self.config.max_history_messages,
                self.config.auto_compaction_enabled,
                self.config.max_tool_result_chars,
            ),
        );

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
