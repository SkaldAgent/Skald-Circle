//! Kernel-driven root turn (phase 2, blueprint §14): `handle_message` builds
//! the turn's `TurnParams` from its fields and drives the `agent-loop` kernel
//! instead of `run_agent_turn`. The translator (`EventTranslator`) is the ONE
//! bus subscriber producing the session's `ServerEvent`s.
//!
//! Sub-agents run on the same kernel via `DelegateTool` (sync); async
//! `execute_task` still rides the legacy interface handler until phase 3.
//! Recovery/resume stays on the old path until phase 3 as well.

use std::sync::Arc;

use agent_loop::activation::ActivateToolsTool;
use agent_loop::delegate::DelegateTool;
use agent_loop::ids::ConversationId;
use agent_loop::manager::{LiveInput, LoopManager, TurnMeta, TurnParams};
use agent_loop::model::{ModelHint, ModelSelector};
use agent_loop::store::{HistoryStore, NewMessage};
use agent_loop::tool::{Extensions, Tool as LoopTool, ToolSet};
use core_api::interface_tool::InterfaceTool;
use core_api::message_meta::MessageMetadata;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::chat_event_bus::ToolCallEvent;
use crate::events::ServerEvent;
use crate::loop_adapters::activation::{SkaldActivationSource, SkaldToolActivator};
use crate::loop_adapters::assembler::SkaldAssembler;
use crate::loop_adapters::builtins::{
    ExecuteTaskAliasTool, LegacyInterfaceTool, SkaldAskUserTool, SkaldHumanChannel,
    UpdateScratchpadTool, WriteTodosTool,
};
use crate::loop_adapters::catalog::SkaldAgentCatalog;
use crate::loop_adapters::gate::ApprovalGate;
use crate::loop_adapters::history::SqliteHistory;
use crate::loop_adapters::hooks::SkaldWritePreviewHook;
use crate::loop_adapters::live_input::PendingLiveInput;
use crate::loop_adapters::preview::PreviewContext;
use crate::loop_adapters::selector::SkaldSelector;
use crate::loop_adapters::system::AgentSystemContext;
use crate::loop_adapters::toolset::{CallerUserId, SkaldToolSet};
use crate::loop_adapters::translate::EventTranslator;
use crate::tools::tool_names as tn;

use super::interface_tools::AgentRunConfig;
use super::{ChatSessionHandler, MAX_AGENT_DEPTH, PendingUserInput, TurnOutcome};

/// Special-cased names handled natively (never legacy-wrapped).
const NATIVE_NAMES: &[&str] = &[tn::ACTIVATE_TOOLS, tn::EXECUTE_TASK];

impl ChatSessionHandler {
    /// Runs the root turn on the `agent-loop` kernel. Same observable contract
    /// as `run_agent_turn` on the root: events over `tx`, `TurnOutcome` back.
    pub(super) async fn run_kernel_turn(
        &self,
        stack_id:       i64,
        config:         &AgentRunConfig,
        user_content:   &str,
        is_synthetic:   bool,
        metadata:       Option<&MessageMetadata>,
        pending_input:  Option<&Arc<dyn PendingUserInput>>,
        tx:             &mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<TurnOutcome> {
        let pool        = self.db.clone();
        let shared_pool = self.shared_pool.clone();
        let conv        = ConversationId::new(format!("session:{}", self.session_id));

        // ── Store ──
        let store = Arc::new(SqliteHistory::new(pool.clone()));

        // ── Selector (root strength from the agent meta, D14) ──
        let strength = crate::agents::load_meta(&config.agent_id)
            .ok()
            .and_then(|m| m.strength);
        let selector: Arc<dyn ModelSelector> =
            Arc::new(SkaldSelector::new(self.llm_manager.clone(), strength));

        // ── Gate ──
        let group_id = self.tool_group_id().await;
        let gate = ApprovalGate::new(
            self.approval.clone(),
            store.clone(),
            self.tools.clone(),
            self.session_id,
            &self.source,
            group_id,
            self.run_context.clone(),
            self.pre_approved.clone(),
            self.auto_deny_approvals.clone(),
            self.context_label.clone(),
            pool.clone(),
            shared_pool.clone(),
            Some(self.fs.clone()),
        );

        // ── Hooks ──
        let preview_hook = Arc::new(SkaldWritePreviewHook::new(PreviewContext {
            pool:        pool.clone(),
            shared_pool: shared_pool.clone(),
            fs:          Some(self.fs.clone()),
        }));

        // ── Manager ──
        let manager = Arc::new(
            LoopManager::builder()
                .models(selector)
                .store(store.clone())
                .gate_arc(Arc::new(gate))
                .hook(preview_hook)
                .max_rounds(self.max_tool_rounds)
                .max_parallel_calls(self.max_parallel_subagents)
                .build()?,
        );

        // ── Catalog + delegate ──
        let config_defs = Arc::new(config.config_tool_defs.clone());
        let catalog = Arc::new(SkaldAgentCatalog::new(
            pool.clone(),
            shared_pool.clone(),
            self.user_id.clone(),
            self.session_id,
            self.source.clone(),
            self.is_interactive,
            self.context_label.clone(),
            self.llm_manager.clone(),
            self.approval.clone(),
            self.clarification.clone(),
            self.mcp.clone(),
            self.tools.clone(),
            config.base_tool_defs.clone(),
            config_defs.clone(),
            config.memory_tools.clone(),
            config.image_tools.clone(),
            config.root_only_tool_names.clone(),
            self.datetime_config.clone(),
            self.max_history_messages,
            self.max_tool_result_chars,
            self.compactor.is_some(),
            Some(self.fs.load()),
            self.run_context.read().await.as_ref().and_then(|rc| rc.project_root.clone()),
        ));
        let delegate = DelegateTool::new(manager.clone(), catalog.clone(), store.clone(), MAX_AGENT_DEPTH as u32);
        catalog.set_delegate(delegate.clone());

        // ── Tool set ──
        let mut native: Vec<Arc<dyn LoopTool>> = Vec::new();
        // activate_tools (root scope — shares the config's grant set so the
        // next round sees the new tools, exactly like today).
        native.push(Arc::new(
            ActivateToolsTool::new(Arc::new(SkaldToolActivator::new(
                pool.clone(),
                self.mcp.clone(),
                config.active_mcp_grants.clone(),
                self.session_id,
                None,
            )))
            .with_definition(super::config::activate_tools_tool_def()),
        ));
        // execute_task: sync → DelegateTool; async → the legacy interface handler.
        {
            let et = native_interface(config, tn::EXECUTE_TASK);
            let (def, handler) = match et {
                Some(it) => (it.definition.clone(), Some(it.handler.clone())),
                None => (legacy_execute_task_def(), None),
            };
            native.push(Arc::new(ExecuteTaskAliasTool::new(
                delegate.clone().with_name(tn::EXECUTE_TASK),
                def,
                handler,
            )));
        }
        native.push(Arc::new(SkaldAskUserTool::new(
            Arc::new(SkaldHumanChannel::new(
                self.clarification.clone(),
                self.session_id,
                &config.agent_id,
                &self.source,
                self.is_interactive,
                self.context_label.clone(),
            )),
            store.clone(),
        )));
        native.push(Arc::new(UpdateScratchpadTool::new(pool.clone(), self.scratchpad_sid())));
        native.push(Arc::new(WriteTodosTool));

        // Legacy interface tools (per-surface, minus the native ones).
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

        let mut toolset = SkaldToolSet::new(
            config.base_tool_defs.clone(),
            config_defs.clone(),
            self.mcp.clone(),
            config.active_mcp_grants.clone(),
            config.memory_tools.clone(),
            config.image_tools.clone(),
            legacy,
            self.tools.all_tools(),
        )
        .with_discovery(self.tool_discovery.clone());
        for t in native {
            toolset = toolset.with_native(t);
        }
        let tools: Arc<dyn ToolSet> = Arc::new(toolset);

        // ── System context ──
        let system = Arc::new(AgentSystemContext {
            agent_id:      config.agent_id.clone(),
            extra_static:  config.extra_system.clone(),
            extra_dynamic: config.extra_system_dynamic.clone(),
            tail_reminder: config.tail_reminder.clone(),
            substitutions: config.system_substitutions.clone(),
            pool:          pool.clone(),
            shared_pool:   shared_pool.clone(),
            user_id:       self.user_id.clone(),
            mcp:           self.mcp.clone(),
            project_root:  self.run_context.read().await.as_ref().and_then(|rc| rc.project_root.clone()),
        });

        // ── Assembler ──
        let assembler = Arc::new(SkaldAssembler {
            pool:                  pool.clone(),
            scratchpad_sid:        self.scratchpad_sid(),
            datetime_config:       self.datetime_config.clone(),
            max_history_messages:  self.max_history_messages,
            max_tool_result_chars: self.max_tool_result_chars,
            compactor_enabled:     self.compactor.is_some(),
            fs:                    Some(self.fs.load()),
            activation: Some(SkaldActivationSource::new(
                pool.clone(),
                self.mcp.clone(),
                config_defs.clone(),
                self.session_id,
                None,
            )),
        });

        // ── Extensions (tool bridge context) ──
        let mut extensions = Extensions::new();
        extensions.insert(pool.clone());
        extensions.insert(self.fs.load());
        extensions.insert(Arc::new(CallerUserId(self.user_id.clone())));

        // ── Live input ──
        let live_input: Option<Arc<dyn LiveInput>> =
            pending_input.map(|p| Arc::new(PendingLiveInput::new(p.clone())) as Arc<dyn LiveInput>);

        // ── Translator ──
        let (translator, shared) = EventTranslator::new(
            tx.clone(),
            self.tools.clone(),
            self.mcp.clone(),
            store.clone(),
        );
        let stop = CancellationToken::new();
        let translator_task = translator.spawn(manager.events(), stop.clone());

        // ── Frame + turn ──
        let frame = store
            .open_frame(&conv, None, agent_loop::store::FrameSpec::root(&config.agent_id))
            .await?;
        // The frame opened at session provisioning is the one the old path
        // used — assert the mapping (defensive; remove once bedded in).
        debug_assert_eq!(frame.get(), stack_id);

        let msg = NewMessage {
            role: agent_loop::store::Role::User,
            content: user_content.to_string(),
            synthetic: is_synthetic,
            reasoning: None,
            metadata: metadata.and_then(|m| serde_json::to_value(m).ok()),
        };
        let params = TurnParams {
            frame,
            agent: config.agent_id.clone(),
            system,
            tools,
            model_hint: ModelHint::name(config.client_name.clone()),
            live_input,
            extensions,
            meta: TurnMeta {
                synthetic: is_synthetic,
                interactive: self.is_interactive,
                ..TurnMeta::default()
            },
            assembler: Some(assembler),
        };

        // Register for /stop, then drive.
        *self.kernel_live.lock().unwrap() = Some((manager.clone(), conv.clone()));
        let handle = manager.start_turn(conv.clone(), msg, params).await
            .map_err(|e| anyhow::anyhow!("kernel turn failed to start: {e}"))?;
        let outcome = handle.join().await;
        *self.kernel_live.lock().unwrap() = None;
        stop.cancel();
        let _ = translator_task.await;

        let shared_state = std::mem::take(&mut *shared.lock().unwrap());

        match outcome? {
            agent_loop::kernel::TurnOutcome::Final { content, message_id, usage, reasoning } => {
                let tool_calls: Vec<ToolCallEvent> = shared_state.tool_calls;
                info!(
                    session_id = self.session_id,
                    user_message_id = ?shared_state.user_message_id,
                    "kernel turn final"
                );
                Ok(TurnOutcome::Final {
                    content,
                    message_id: message_id.get(),
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    truncated: usage.truncated,
                    reasoning_content: reasoning,
                    tool_calls,
                })
            }
            agent_loop::kernel::TurnOutcome::Cancelled => Ok(TurnOutcome::Cancelled),
            agent_loop::kernel::TurnOutcome::Exhausted => Ok(TurnOutcome::Exhausted),
        }
    }

    /// `/stop` for the kernel-driven turn: cancels the live loop (the legacy
    /// `current_cancel` path keeps covering resume/recovery).
    pub(super) fn cancel_kernel_turn(&self) {
        let live = self.kernel_live.lock().unwrap().clone();
        if let Some((manager, conv)) = live {
            manager.cancel(&conv);
        }
    }
}

/// Finds an interface tool by name in the run config.
fn native_interface(config: &AgentRunConfig, name: &str) -> Option<InterfaceTool> {
    config
        .interface_tools
        .iter()
        .find(|it| it.definition["function"]["name"].as_str() == Some(name))
        .cloned()
}

/// Fallback definition for `execute_task` when no interface handler was
/// injected (non-interactive sessions): mirrors the injected one.
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
