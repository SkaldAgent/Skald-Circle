//! The session's turns, driven by the `agent-loop` kernel (blueprint §14).
//!
//! Everything shared lives on the user's `UserLoopRuntime` (manager, store,
//! gate, catalog, delegate); this only assembles the turn's own state —
//! [`TurnScope`] plus the run config — and reads the outcome back. The
//! translator (`EventTranslator`) is the ONE bus subscriber producing the
//! session's `ServerEvent`s.
//!
//! Three entry points, one path:
//!
//! - [`run_kernel_turn`](ChatSessionHandler::run_kernel_turn) — a user message.
//!   It repairs first: a call left dangling by a crash is resolved before the
//!   new turn appends anything.
//! - [`recover_turn`](ChatSessionHandler::recover_turn) — no new message:
//!   continue a turn that was interrupted (a client reconnecting, a background
//!   job, a decision taken out of band).
//! - [`resolve_pending_call`](ChatSessionHandler::resolve_pending_call) — a
//!   human answered an approval nothing is waiting on anymore.
//!
//! Sub-agents run on the same kernel via `DelegateTool`, sync and async alike.

use std::collections::HashMap;
use std::sync::Arc;

use agent_loop::recovery::{HumanDecision, RecoveryPolicy, RecoveryReport};
use agent_loop::store::{NewMessage, Role};
use core_api::message_meta::MessageMetadata;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::chat_event_bus::ToolCallEvent;
use crate::events::ServerEvent;
use crate::loop_adapters::runtime::{TurnInputs, UserLoopRuntime};
use crate::loop_adapters::scope::TurnScope;
use crate::loop_adapters::translate::EventTranslator;

use super::interface_tools::{AgentRunConfig, InterfaceTool};
use super::{ChatSessionHandler, PendingUserInput, TurnOutcome};

/// What Skald does with a conversation a crash left mid-flight.
///
/// `ReExecute` + `ReAsk` is the historical behavior: an interrupted call runs
/// again and an approval card reappears — except where the tool itself says
/// otherwise (`execute_cmd` declares `MarkInterrupted`, D7: a command may
/// already have had its effect).
fn policy() -> RecoveryPolicy {
    RecoveryPolicy {
        interrupted_text: "Error: this tool call was interrupted by a restart and was NOT \
                           re-run automatically (its effects may be partial). Re-run it if \
                           the task still needs it."
            .to_string(),
        ..RecoveryPolicy::default()
    }
}

impl ChatSessionHandler {
    /// Runs the root turn on the `agent-loop` kernel: events over `tx`, the
    /// turn's outcome back.
    pub(super) async fn run_kernel_turn(
        &self,
        config:        &AgentRunConfig,
        user_content:  &str,
        is_synthetic:  bool,
        metadata:      Option<&MessageMetadata>,
        pending_input: Option<&Arc<dyn PendingUserInput>>,
        tx:            &mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<TurnOutcome> {
        let rt = self.loop_runtime.clone();
        let conv = UserLoopRuntime::conversation(self.session_id);

        // ── The turn's own state, read by the long-lived gate and catalog ──
        let scope = Arc::new(self.turn_scope(config).await);

        // ── The one bus subscriber for this session's events ──
        let (translator, shared) = EventTranslator::new(
            tx.clone(),
            conv.clone(),
            self.tools.clone(),
            self.mcp.clone(),
            rt.store().clone(),
        );
        let stop = CancellationToken::new();
        let translator_task = translator.spawn(rt.manager().events(), stop.clone());

        // ── Drive ──
        let mut params = rt
            .turn_params(TurnInputs { scope, config, live_input: pending_input.cloned() })
            .await?;
        params.meta.synthetic = is_synthetic;

        // A previous turn may have died with a call still in flight. Repair it
        // before appending anything: the model must never be shown a call with
        // no result, and the resumed result belongs to the OLD turn, so it has
        // to land before the new message. This does not re-drive that turn —
        // the user has moved on.
        let repaired = self.recovery().repair(&conv, &params).await?;
        if repaired != agent_loop::recovery::RecoveryReport::default() {
            info!(session_id = self.session_id, ?repaired, "repaired an interrupted turn");
        }

        let msg = NewMessage {
            role:      Role::User,
            content:   user_content.to_string(),
            synthetic: is_synthetic,
            reasoning: None,
            metadata:  metadata.and_then(|m| serde_json::to_value(m).ok()),
        };

        let outcome = rt
            .manager()
            .start_turn(conv, msg, params)
            .await
            .map_err(|e| anyhow::anyhow!("kernel turn failed to start: {e}"))?
            .join()
            .await;

        // Let the translator drain what the kernel emitted, then stop it.
        stop.cancel();
        let _ = translator_task.await;

        let shared_state = std::mem::take(&mut *shared.lock().unwrap());

        match outcome? {
            agent_loop::kernel::TurnOutcome::Final { content, message_id, usage, .. } => {
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
                    tool_calls,
                })
            }
            agent_loop::kernel::TurnOutcome::Cancelled => Ok(TurnOutcome::Cancelled),
            agent_loop::kernel::TurnOutcome::Exhausted => Ok(TurnOutcome::Exhausted),
        }
    }

    /// Continues a turn nobody is driving: a client reconnecting to a session
    /// that was mid-tool when the process died, a background job's parent, or a
    /// conversation woken by an async result.
    ///
    /// No new user message — the history already says what to do. Sub-agent
    /// frames cascade back to the root, each running as **its own** agent.
    pub async fn recover_turn(
        &self,
        interface_tools: Vec<InterfaceTool>,
        tx:              mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<()> {
        let _guard = self.processing.lock().await;
        let report = self.drive_recovery(interface_tools, tx, None).await?;
        info!(session_id = self.session_id, ?report, "recover_turn done");
        Ok(())
    }

    /// Applies a human's decision to a call that has no loop waiting on it — an
    /// approval card answered after a restart, or from the Inbox — then
    /// continues the conversation.
    ///
    /// Approval **skips the gate** (the human just decided) but not the
    /// context: the tool runs with this session's `ToolContext`, so a write
    /// lands in the caller's workspace and a command in their container, never
    /// on the host (blueprint §6).
    pub async fn resolve_pending_call(
        &self,
        call:            i64,
        decision:        HumanDecision,
        interface_tools: Vec<InterfaceTool>,
        tx:              mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<()> {
        let _guard = self.processing.lock().await;
        let report = self.drive_recovery(interface_tools, tx, Some((call, decision))).await?;
        info!(session_id = self.session_id, call, ?report, "resolve_pending_call done");
        Ok(())
    }

    /// The shared body of the two entry points above: build the root turn's
    /// parameters, subscribe the translator, run recovery (optionally applying
    /// a human decision first), drain the events.
    async fn drive_recovery(
        &self,
        interface_tools: Vec<InterfaceTool>,
        tx:              mpsc::Sender<ServerEvent>,
        decision:        Option<(i64, HumanDecision)>,
    ) -> anyhow::Result<RecoveryReport> {
        let rt = self.loop_runtime.clone();
        let conv = UserLoopRuntime::conversation(self.session_id);

        let mut config = self
            .build_agent_config(None, None, None, interface_tools, HashMap::new())
            .await?;
        // The tail reminder belongs to a fresh user message, not to finishing
        // work that was already under way.
        config.tail_reminder = None;
        let scope = Arc::new(self.turn_scope(&config).await);

        let (translator, _shared) = EventTranslator::new(
            tx.clone(),
            conv.clone(),
            self.tools.clone(),
            self.mcp.clone(),
            rt.store().clone(),
        );
        let stop = CancellationToken::new();
        let translator_task = translator.spawn(rt.manager().events(), stop.clone());

        let params = rt
            .turn_params(TurnInputs { scope, config: &config, live_input: None })
            .await?;

        let result = match decision {
            Some((call, decision)) => {
                rt.manager()
                    .resolve_pending(
                        agent_loop::ids::ToolCallId(call),
                        decision,
                        rt.catalog().clone(),
                        &params,
                    )
                    .await
            }
            None => self.recovery().run(&conv, &params).await,
        };

        stop.cancel();
        let _ = translator_task.await;
        result
    }

    /// Recovery bound to this user's manager, with Skald's policy.
    fn recovery(&self) -> agent_loop::recovery::Recovery {
        let rt = &self.loop_runtime;
        rt.manager().recovery(rt.catalog().clone(), policy())
    }

    /// The turn's scope: identity, the live cells the gate watches, and the tool
    /// material a sub-agent derives its own set from.
    async fn turn_scope(&self, config: &AgentRunConfig) -> TurnScope {
        TurnScope {
            session_id:     self.session_id,
            source:         self.source.clone(),
            is_interactive: self.is_interactive,
            agent_id:       config.agent_id.clone(),
            scratchpad_sid: self.scratchpad_sid(),
            project_root:   self
                .run_context
                .read()
                .await
                .as_ref()
                .and_then(|rc| rc.project_root.clone()),
            context_label:  self.context_label.clone(),
            run_context:    self.run_context.clone(),
            group_id:       self.tool_group_id().await,
            pre_approved:   self.pre_approved.clone(),
            auto_deny:      self.auto_deny_approvals.clone(),
            grants:         config.active_mcp_grants.clone(),
            base_defs:      Arc::new(config.base_tool_defs.clone()),
            config_defs:    Arc::new(config.config_tool_defs.clone()),
            memory_tools:   Arc::new(config.memory_tools.clone()),
            image_tools:    Arc::new(config.image_tools.clone()),
            root_only:      Arc::new(config.root_only_tool_names.clone()),
        }
    }

    /// `/stop` for the kernel-driven turn: the manager cancels the live loop of
    /// this conversation (the legacy `current_cancel` path still covers
    /// resume/recovery).
    pub(super) fn cancel_kernel_turn(&self) {
        self.loop_runtime
            .manager()
            .cancel(&UserLoopRuntime::conversation(self.session_id));
    }
}
