//! Restart recovery (blueprint §8) — turning a half-written conversation back
//! into a well-formed one, then running a **normal loop** on it.
//!
//! There is no "recovery mode" in the kernel. Every state transition is written
//! the instant it happens (see [`crate::store`]), so a crash loses RAM — the
//! approval oneshot, the cancellation token — never the truth. What it leaves
//! behind is a store that a model would choke on: calls with no result, a child
//! frame whose answer nobody propagated, a half-run parallel batch. This module
//! repairs exactly those, then hands the frame to the same `LlmLoop` a live turn
//! uses.
//!
//! The order matters and mirrors `resume.rs`, the path this replaces:
//!
//! 1. **Reap** an interrupted parallel batch (≥2 active frames at one depth).
//! 2. **Resolve** the deepest active frame's non-terminal calls, by policy and
//!    by each tool's [`RestartHint`].
//! 3. **Un-wedge**: a child that finished but never told its parent.
//! 4. **Cascade**: run the frame, resolve its parent's call with the result,
//!    close it, walk up — every frame with **its own** agent's config (B3), read
//!    from the catalog, never the root's.

use std::collections::HashMap;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::delegate::{AgentCatalog, FilteredToolSet};
use crate::events::{EventSink, LoopEvent, PendingToolCall};
use crate::ids::{ConversationId, FrameId};
use crate::kernel::{PreExecution, TurnOutcome};
use crate::manager::{LoopManager, LoopParams, TurnMeta, TurnParams};
use crate::store::{CallOutcome, CallState, FrameRecord, Role, StoredCall};
use crate::tool::{ExecutionOutcome, RestartHint, ToolCtx, ToolSet, drive_execution};

// ── Policy ───────────────────────────────────────────────────────────────────

/// What to do with a call that was `Running` when the process died.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunningPolicy {
    /// Re-gate and re-execute, unless the tool's own [`RestartHint`] says
    /// otherwise (which always wins: only the tool knows if it is idempotent).
    #[default]
    ReExecute,
    /// Never re-run: resolve every interrupted call as failed.
    MarkInterrupted,
}

/// What to do with a call that was waiting on a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PendingPolicy {
    /// Ask again — the approval card reappears (today's behavior).
    #[default]
    ReAsk,
    /// Leave it pending for an out-of-band decision
    /// ([`LoopManager::resolve_pending`]), and stop: the frame cannot run with
    /// an unanswered call in it.
    LeavePending,
}

#[derive(Debug, Clone)]
pub struct RecoveryPolicy {
    pub on_running:        RunningPolicy,
    pub on_awaiting_human: PendingPolicy,
    /// Recorded on a call that is not re-run.
    pub interrupted_text:  String,
    /// Recorded on the delegating call of a reaped parallel batch.
    pub batch_reaped_text: String,
}

impl Default for RecoveryPolicy {
    fn default() -> Self {
        Self {
            on_running:        RunningPolicy::default(),
            on_awaiting_human: PendingPolicy::default(),
            interrupted_text:  "Tool call interrupted by a restart.".to_string(),
            batch_reaped_text: "Sub-agent interrupted by restart (parallel batch).".to_string(),
        }
    }
}

/// What a recovery pass did — logged by hosts, asserted by tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub frames_resumed:   usize,
    pub calls_reexecuted: usize,
    pub calls_failed:     usize,
    pub batches_reaped:   usize,
    /// A call was left `AwaitingHuman`: the conversation waits for a decision.
    pub left_pending:     bool,
}

// ── Recovery ─────────────────────────────────────────────────────────────────

pub struct Recovery {
    manager: Arc<LoopManager>,
    catalog: Arc<dyn AgentCatalog>,
    policy:  RecoveryPolicy,
}

impl Recovery {
    pub fn new(
        manager: Arc<LoopManager>,
        catalog: Arc<dyn AgentCatalog>,
        policy:  RecoveryPolicy,
    ) -> Self {
        Self { manager, catalog, policy }
    }

    /// Recover one conversation. `root` is what the **root** frame runs with —
    /// the host's own turn parameters, since no catalog describes the entry
    /// agent; `root.frame` must be that root frame, and `root.live_input` is
    /// ignored (a recovery is not a live turn).
    ///
    /// Refuses while a loop is already live on the conversation: that loop is
    /// already the thing driving it.
    pub async fn run(
        &self,
        conv: &ConversationId,
        root: &TurnParams,
    ) -> crate::Result<RecoveryReport> {
        let Some(claim) = self.manager.claim(conv, root.frame, &root.agent) else {
            info!(%conv, "recovery: a loop is already running — nothing to do");
            return Ok(RecoveryReport::default());
        };
        let token = claim.token();
        let events = self.manager.sink_for(conv.clone());
        let store = self.manager.store();
        let mut report = RecoveryReport::default();

        // ── 1. reap an interrupted parallel batch ──
        self.reap_batches(conv, &mut report).await?;

        // ── 2. the deepest active frame is where the conversation stopped ──
        let Some(mut frame) = store.deepest_active(conv).await? else {
            info!(%conv, "recovery: no active frame — nothing to resume");
            return Ok(report);
        };

        let mut params = self.params_for(&frame, root, conv).await?;
        let pending = self
            .resolve_frame_calls(conv, &frame, &params, &token, &events, &mut report)
            .await?;
        if report.left_pending {
            return Ok(report);
        }

        // ── 3. un-wedge: a finished child whose result never reached its parent ──
        let mut outcome = match self.completed_without_propagating(&frame, pending).await? {
            Some(o) => o,
            None => {
                report.frames_resumed += 1;
                self.run_frame(&params, &token, conv, frame.id, frame.parent).await?
            }
        };

        // ── 4. cascade to the root ──
        while let Some(parent_call) = frame.spec.parent_call {
            let result = child_result(&outcome, &frame.spec.agent);
            match &result {
                Ok(text)  => store.resolve_call(parent_call, &CallOutcome::Completed(
                    crate::tool::ToolOutput::Text(text.clone()),
                )).await?,
                Err(text) => store.resolve_call(parent_call, &CallOutcome::Failed(text.clone())).await?,
            }
            let (text, failed) = match result {
                Ok(t)  => (t, false),
                Err(t) => (t, true),
            };
            self.catalog.on_child_closed(frame.id).await;
            store.close_frame(frame.id).await?;

            let parent = match store.frame_of_call(parent_call).await? {
                Some(p) => p,
                None => {
                    warn!(%conv, call = %parent_call, "recovery: the call's frame is gone");
                    break;
                }
            };
            events.emit(frame.id, Some(parent.id), LoopEvent::AgentFinished {
                frame:          frame.id,
                agent:          frame.spec.agent.clone(),
                result_preview: crate::delegate::preview_truncate(&text, 500),
                parent_agent:   parent.spec.agent.clone(),
            });
            events.emit(parent.id, parent.parent, LoopEvent::ToolCallFinished {
                id: parent_call,
                outcome: if failed {
                    CallOutcome::Failed(text)
                } else {
                    CallOutcome::Completed(crate::tool::ToolOutput::Text(text))
                },
            });

            frame  = parent;
            params = self.params_for(&frame, root, conv).await?;
            self.resolve_frame_calls(conv, &frame, &params, &token, &events, &mut report)
                .await?;
            if report.left_pending {
                return Ok(report);
            }
            report.frames_resumed += 1;
            outcome = self.run_frame(&params, &token, conv, frame.id, frame.parent).await?;
        }

        drop(claim);
        Ok(report)
    }

    /// Make the store well-formed **without continuing the conversation**: reap
    /// an interrupted batch, resolve the deepest frame's dangling calls.
    ///
    /// This is what a host runs before starting a *new* turn on a session that
    /// died mid-tool: the user has something else to say, so nothing should
    /// re-drive the old turn, but the model must not be shown a call with no
    /// result. Unlike [`Self::run`] it does not claim the conversation — the
    /// caller is already inside its own turn.
    pub async fn repair(
        &self,
        conv: &ConversationId,
        root: &TurnParams,
    ) -> crate::Result<RecoveryReport> {
        let mut report = RecoveryReport::default();
        self.reap_batches(conv, &mut report).await?;
        if let Some(frame) = self.manager.store().deepest_active(conv).await? {
            let params = self.params_for(&frame, root, conv).await?;
            let token = CancellationToken::new();
            let events = self.manager.sink_for(conv.clone());
            self.resolve_frame_calls(conv, &frame, &params, &token, &events, &mut report)
                .await?;
        }
        Ok(report)
    }

    /// Two or more active frames at one depth can only be a concurrent batch
    /// caught mid-flight (a linear stack has at most one per depth). Recovering
    /// it properly would mean re-driving several siblings; instead the batch is
    /// pruned — deliberately lossy — and the parent continues with the failures
    /// in view.
    async fn reap_batches(
        &self,
        conv:   &ConversationId,
        report: &mut RecoveryReport,
    ) -> crate::Result<()> {
        let store = self.manager.store();
        let active = store.active_frames(conv).await?;
        let Some(d_min) = shallowest_parallel_depth(&active) else {
            return Ok(());
        };
        warn!(%conv, depth = d_min, "recovery: reaping an interrupted parallel batch");
        for frame in active.iter().filter(|f| f.spec.depth >= d_min) {
            if let Some(parent_call) = frame.spec.parent_call {
                let _ = store
                    .resolve_call(
                        parent_call,
                        &CallOutcome::Failed(self.policy.batch_reaped_text.clone()),
                    )
                    .await;
            }
            let _ = store.close_frame(frame.id).await;
        }
        report.batches_reaped += 1;
        Ok(())
    }

    /// Runs one frame's loop to completion, through the manager (so the turn is
    /// an ordinary loop — same kernel, same events, same rules).
    async fn run_frame(
        &self,
        params: &LoopParams,
        token:  &CancellationToken,
        conv:   &ConversationId,
        frame:  FrameId,
        parent: Option<FrameId>,
    ) -> crate::Result<TurnOutcome> {
        let handle = self
            .manager
            .start_loop(clone_params(params, conv, frame, parent, Some(token.clone())))
            .await
            .map_err(|e| anyhow::anyhow!("recovery: {e}"))?;
        handle.join().await
    }

    /// Every non-terminal call of a frame, resolved per policy. Returns whether
    /// anything at all was pending (the un-wedge check needs to know).
    async fn resolve_frame_calls(
        &self,
        conv:   &ConversationId,
        frame:  &FrameRecord,
        params: &LoopParams,
        token:  &CancellationToken,
        events: &EventSink,
        report: &mut RecoveryReport,
    ) -> crate::Result<bool> {
        let store = self.manager.store();
        let calls = store
            .calls_in_state(frame.id, &[CallState::Running, CallState::AwaitingHuman])
            .await?;
        if calls.is_empty() {
            return Ok(false);
        }

        // A call that spawned a frame is the cascade's business: its result is
        // the child's answer, not a re-execution. Structural, not by name — a
        // host may register the delegate under any number of aliases.
        let children = store.active_frames(conv).await?;
        let spawned = |call: &StoredCall| {
            children.iter().any(|f| f.spec.parent_call == Some(call.id))
        };

        for call in &calls {
            if spawned(call) {
                info!(call = %call.id, "recovery: sub-agent call left to the cascade");
                continue;
            }

            let hint = params
                .tools
                .find(&call.name)
                .map(|t| t.restart_hint())
                .unwrap_or_default();
            let re_execute = match call.state {
                CallState::AwaitingHuman => match self.policy.on_awaiting_human {
                    PendingPolicy::ReAsk => true,
                    PendingPolicy::LeavePending => {
                        info!(call = %call.id, "recovery: leaving the call pending for a decision");
                        report.left_pending = true;
                        return Ok(true);
                    }
                },
                // The tool's own hint wins: only it knows whether re-running is
                // safe (a shell command may already have had its effect).
                _ => {
                    self.policy.on_running == RunningPolicy::ReExecute
                        && hint == RestartHint::ReExecute
                }
            };

            if !re_execute {
                store
                    .resolve_call(
                        call.id,
                        &CallOutcome::Failed(self.policy.interrupted_text.clone()),
                    )
                    .await?;
                events.emit(frame.id, frame.parent, LoopEvent::ToolCallFinished {
                    id: call.id,
                    outcome: CallOutcome::Failed(self.policy.interrupted_text.clone()),
                });
                report.calls_failed += 1;
                continue;
            }

            if self.re_execute(call, params, token, events, frame).await? {
                report.calls_reexecuted += 1;
            } else {
                // Suspended again (the human is still not there, or the channel
                // closed): the call stays AwaitingHuman for the next attempt.
                report.left_pending = true;
                return Ok(true);
            }
        }
        Ok(true)
    }

    /// Re-runs one call through the **normal** path — gate, hooks, tool — so a
    /// rule change since the crash applies and the approval card reappears.
    /// `Ok(false)` = it suspended again and must be left pending.
    async fn re_execute(
        &self,
        call:   &StoredCall,
        params: &LoopParams,
        token:  &CancellationToken,
        events: &EventSink,
        frame:  &FrameRecord,
    ) -> crate::Result<bool> {
        let ptc = PendingToolCall {
            id:          call.id,
            message_id:  call.message_id,
            provider_id: Some(call.provider_id.clone()).filter(|s| !s.is_empty()),
            name:        call.name.clone(),
            arguments:   call.arguments.clone(),
        };
        events.emit(frame.id, frame.parent, LoopEvent::ToolCallStarted {
            id:         ptc.id,
            message_id: ptc.message_id,
            name:       ptc.name.clone(),
            args:       ptc.arguments.clone(),
        });

        let deps = self.manager.deps();
        match crate::kernel::pre_execution(deps, params, events, token, &ptc).await? {
            PreExecution::Run(tool) => {
                let ctx = ToolCtx {
                    conversation: params.conversation.clone(),
                    frame:        params.frame,
                    agent:        params.agent.clone(),
                    call_id:      ptc.id,
                    cancel:       token.clone(),
                    extensions:   crate::kernel::tool_extensions(params, events),
                };
                let exec = tool.start(ptc.arguments.clone(), &ctx);
                match drive_execution(&*exec, token).await {
                    ExecutionOutcome::Suspended => Ok(false),
                    outcome => {
                        crate::kernel::record_outcome(
                            deps,
                            params,
                            events,
                            &self.manager.store(),
                            &ptc,
                            outcome.into_call_outcome(),
                        )
                        .await?;
                        Ok(true)
                    }
                }
            }
            PreExecution::Resolved(outcome) => {
                crate::kernel::record_outcome(
                    deps, params, events, &self.manager.store(), &ptc, outcome,
                )
                .await?;
                Ok(true)
            }
            PreExecution::Suspended => Ok(false),
            PreExecution::TurnCancelled => Ok(false),
        }
    }

    /// The wedge case: nothing was pending and the frame's last message is a
    /// plain assistant reply — its turn finished, and the process died before
    /// the result reached the parent. Re-running the model would ask it to
    /// answer a question it already answered, so the stored answer is used as
    /// the outcome and only the propagation is redone.
    ///
    /// On the ROOT frame the same shape means the turn is simply complete.
    async fn completed_without_propagating(
        &self,
        frame:       &FrameRecord,
        had_pending: bool,
    ) -> crate::Result<Option<TurnOutcome>> {
        if had_pending {
            return Ok(None);
        }
        let Some(last) = self.manager.store().last(frame.id).await? else {
            return Ok(None);
        };
        if last.role != Role::Assistant || !last.calls.is_empty() {
            return Ok(None);
        }
        Ok(Some(TurnOutcome::Final {
            content:    last.content,
            message_id: last.id,
            usage:      last.usage,
            reasoning:  last.reasoning,
        }))
    }

    /// The parameters one frame runs with: the host's for the root, the
    /// catalog's for every other (B3 — a resumed sub-agent is ITS agent, with
    /// its prompt, its tools and its model).
    async fn params_for(
        &self,
        frame: &FrameRecord,
        root:  &TurnParams,
        conv:  &ConversationId,
    ) -> crate::Result<LoopParams> {
        let mut params = clone_params_from_turn(root, conv, frame.id, frame.parent);
        if frame.spec.parent_call.is_none() {
            return Ok(params);
        }

        let ctx = ToolCtx {
            conversation: conv.clone(),
            frame:        frame.id,
            agent:        frame.spec.agent.clone(),
            // The call that spawned this frame — the same handle the live
            // dispatch had.
            call_id:      frame.spec.parent_call.unwrap(),
            cancel:       CancellationToken::new(),
            extensions:   root.extensions.clone(),
        };
        let profile = self.catalog.get(&frame.spec.agent, frame.id, &ctx).await?;

        params.agent  = frame.spec.agent.clone();
        params.system = profile.context;
        params.tools  = match profile.toolset {
            Some(ts) => ts,
            None => Arc::new(FilteredToolSet::derive(root.tools.clone(), &profile.tools))
                as Arc<dyn ToolSet>,
        };
        params.model_hint = profile.model.unwrap_or_default();
        params.selector   = profile.selector;
        params.assembler  = profile.assembler;
        params.meta       = TurnMeta { user_message: frame.spec.prompt.clone(), ..root.meta.clone() };
        Ok(params)
    }
}

// ── resolve_pending (blueprint §8.5) ─────────────────────────────────────────

/// A human's answer to a call that was waiting for one.
#[derive(Debug, Clone)]
pub enum HumanDecision {
    Approved,
    Rejected { reason: String },
}

/// Apply a human decision to a call nothing is driving anymore — the approval
/// card answered after a restart, or from the Inbox.
///
/// Approval **skips the gate**: the human is the gate, and re-running the rules
/// would ask them again. The call is executed through the normal tool path
/// (with the frame's own context, so a write lands in the caller's workspace,
/// never the server's cwd), then the conversation is recovered so the model
/// reads the result.
pub(crate) async fn resolve_pending(
    manager:  &Arc<LoopManager>,
    call_id:  crate::ids::ToolCallId,
    decision: HumanDecision,
    catalog:  Arc<dyn AgentCatalog>,
    root:     &TurnParams,
) -> crate::Result<RecoveryReport> {
    let store = manager.store();
    let call = store
        .get_call(call_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("resolve_pending: call {call_id} not found"))?;
    if call.state.is_terminal() {
        info!(call = %call_id, state = ?call.state, "resolve_pending: already resolved");
        return Ok(RecoveryReport::default());
    }
    let frame = store
        .frame_of_call(call_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("resolve_pending: no frame for call {call_id}"))?;
    let conv = frame.conversation.clone();

    match decision {
        HumanDecision::Rejected { reason } => {
            store.resolve_call(call_id, &CallOutcome::Rejected { reason: reason.clone() }).await?;
            manager.sink_for(conv.clone()).emit(frame.id, frame.parent, LoopEvent::ToolCallFinished {
                id:      call_id,
                outcome: CallOutcome::Rejected { reason },
            });
        }
        HumanDecision::Approved => {
            // Claimed for the execution only: the recovery below takes its own.
            let outcome = {
                let Some(claim) = manager.claim(&conv, frame.id, &frame.spec.agent) else {
                    anyhow::bail!("resolve_pending: a loop is already running on {conv}");
                };
                let token = claim.token();
                let events = manager.sink_for(conv.clone());
                let params = clone_params_from_turn(root, &conv, frame.id, frame.parent);
                let ext = crate::kernel::tool_extensions(&params, &events);

                match params.tools.find(&call.name) {
                    Some(tool) => {
                        let ctx = ToolCtx {
                            conversation: conv.clone(),
                            frame:        frame.id,
                            agent:        frame.spec.agent.clone(),
                            call_id,
                            cancel:       token.clone(),
                            extensions:   ext,
                        };
                        let exec = tool.start(call.arguments.clone(), &ctx);
                        match drive_execution(&*exec, &token).await {
                            // Suspending again would need another human: leave
                            // it pending rather than resolving it as cancelled.
                            ExecutionOutcome::Suspended => None,
                            outcome => Some(outcome.into_call_outcome()),
                        }
                    }
                    None => Some(CallOutcome::Failed(format!(
                        "unknown tool '{}' (not in this turn's tool set)",
                        call.name
                    ))),
                }
            };

            let Some(outcome) = outcome else {
                return Ok(RecoveryReport { left_pending: true, ..RecoveryReport::default() });
            };
            store.resolve_call(call_id, &outcome).await?;
            manager.sink_for(conv.clone()).emit(frame.id, frame.parent, LoopEvent::ToolCallFinished {
                id: call_id,
                outcome,
            });
        }
    }

    // The history is well-formed again: a normal recovery continues the turn.
    Recovery::new(manager.clone(), catalog, RecoveryPolicy::default())
        .run(&conv, root)
        .await
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// The text a finished child propagates to its parent's call — `Err` when the
/// child did not produce an answer.
fn child_result(outcome: &TurnOutcome, agent: &str) -> Result<String, String> {
    match outcome {
        TurnOutcome::Final { content, .. } => Ok(content.clone()),
        TurnOutcome::Cancelled => Err(format!("Sub-agent `{agent}` was cancelled.")),
        TurnOutcome::Exhausted => Err(format!("Sub-agent `{agent}` exhausted tool-call rounds.")),
    }
}

fn clone_params_from_turn(
    root:   &TurnParams,
    conv:   &ConversationId,
    frame:  FrameId,
    parent: Option<FrameId>,
) -> LoopParams {
    LoopParams {
        conversation: conv.clone(),
        frame,
        parent_frame: parent,
        agent:        root.agent.clone(),
        system:       root.system.clone(),
        tools:        root.tools.clone(),
        model_hint:   root.model_hint.clone(),
        selector:     root.selector.clone(),
        token:        None,
        // A recovery is not a live turn: no live input, and no tail reminder
        // semantics — the host decides that when it builds `root`.
        live_input:   None,
        extensions:   root.extensions.clone(),
        meta:         root.meta.clone(),
        assembler:    root.assembler.clone(),
    }
}

fn clone_params(
    p:      &LoopParams,
    conv:   &ConversationId,
    frame:  FrameId,
    parent: Option<FrameId>,
    token:  Option<CancellationToken>,
) -> LoopParams {
    LoopParams {
        conversation: conv.clone(),
        frame,
        parent_frame: parent,
        agent:        p.agent.clone(),
        system:       p.system.clone(),
        tools:        p.tools.clone(),
        model_hint:   p.model_hint.clone(),
        selector:     p.selector.clone(),
        token,
        live_input:   None,
        extensions:   p.extensions.clone(),
        meta:         p.meta.clone(),
        assembler:    p.assembler.clone(),
    }
}

/// Shallowest depth holding more than one active frame — the top of an
/// interrupted parallel batch. `None` for a linear stack, where every depth has
/// at most one active frame. Pure (see tests).
pub fn shallowest_parallel_depth(active: &[FrameRecord]) -> Option<u32> {
    let mut by_depth: HashMap<u32, usize> = HashMap::new();
    for f in active {
        *by_depth.entry(f.spec.depth).or_default() += 1;
    }
    by_depth
        .iter()
        .filter_map(|(depth, count)| (*count > 1).then_some(*depth))
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ToolCallId;
    use crate::store::FrameSpec;

    fn frame(id: i64, depth: u32, parent_call: Option<i64>) -> FrameRecord {
        FrameRecord {
            id:           FrameId(id),
            conversation: ConversationId::new("c"),
            parent:       None,
            spec:         FrameSpec {
                agent: "agent".into(),
                prompt: None,
                depth,
                parent_call: parent_call.map(ToolCallId),
                meta: serde_json::Value::Null,
            },
            active:       true,
        }
    }

    #[test]
    fn linear_stack_is_not_a_batch() {
        let frames = vec![frame(1, 0, None), frame(2, 1, Some(10)), frame(3, 2, Some(20))];
        assert_eq!(shallowest_parallel_depth(&frames), None);
        assert_eq!(shallowest_parallel_depth(&[]), None);
    }

    #[test]
    fn detects_shallowest_multi_frame_depth() {
        // Two siblings at depth 1 (parallel batch) plus a grandchild at depth 2.
        let frames = vec![
            frame(1, 0, None),
            frame(2, 1, Some(10)),
            frame(3, 1, Some(11)),
            frame(4, 2, Some(30)),
        ];
        assert_eq!(shallowest_parallel_depth(&frames), Some(1));
    }

    #[test]
    fn detects_deeper_batch_when_upper_levels_linear() {
        let frames = vec![
            frame(1, 0, None),
            frame(2, 1, Some(10)),
            frame(3, 2, Some(20)),
            frame(4, 2, Some(21)),
        ];
        assert_eq!(shallowest_parallel_depth(&frames), Some(2));
    }
}
