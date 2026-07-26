//! The kernel — `LlmLoop`. It owns ONLY control flow: round loop, model
//! fallback, tool fan-out, recording. It knows nothing about agents, approval
//! rules, MCP, compaction or recovery (blueprint §5).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::anyhow;
use futures::StreamExt as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::context::{AssembleInput, ContextAssembler};
use crate::events::{EventSink, LoopEvent, PendingToolCall};
use crate::gate::{Gate, GateDecision, PendingCall};
use crate::hooks::{HookCtx, HookVerdict, LoopHooks};
use crate::ids::{FrameId, MessageId, ModelId};
use crate::manager::LoopParams;
use crate::model::{
    ModelHandle, ModelRequest, ModelResponse, ModelSelector, RetryPolicy, StreamDelta, Usage,
};
use crate::store::{CallOutcome, HistoryStore, NewCall, NewMessage};
use crate::tool::{ExecutionOutcome, ToolCtx, drive_execution};

/// The terminal outcome of a turn.
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    Final {
        content:    String,
        message_id: MessageId,
        usage:      Usage,
        reasoning:  Option<String>,
    },
    Cancelled,
    /// Round budget exhausted.
    Exhausted,
}

/// Shared dependencies the manager hands to every loop.
pub(crate) struct KernelDeps {
    pub(crate) models:            Arc<dyn ModelSelector>,
    pub(crate) store:             Arc<dyn HistoryStore>,
    pub(crate) gate:              Arc<dyn Gate>,
    pub(crate) hooks:             Vec<Arc<dyn LoopHooks>>,
    pub(crate) assembler:         Arc<dyn ContextAssembler>,
    pub(crate) max_rounds:        usize,
    pub(crate) max_parallel_calls: usize,
    pub(crate) retry:             RetryPolicy,
}

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Correlation id for host-side payload logging (one per attempt).
fn mint_request_id() -> String {
    let n = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:032x}-{n:08x}")
}

/// Run one loop to completion. Spawned by the manager; the `token` is cloned
/// by value through the whole call tree — never re-read from a field mid-turn.
pub(crate) async fn run(
    deps:   Arc<KernelDeps>,
    params: LoopParams,
    token:  CancellationToken,
    events: EventSink,
) -> crate::Result<TurnOutcome> {
    let frame = params.frame;
    let parent = params.parent_frame;
    let store = deps.store.clone();
    let assembler = params.assembler.clone().unwrap_or_else(|| deps.assembler.clone());

    let hook_ctx = || HookCtx {
        conversation: params.conversation.clone(),
        frame,
        agent: params.agent.clone(),
        store: store.clone(),
        events: events.clone(),
    };

    // ToolCtx extensions: host extensions + the event sink, so shipped tools
    // (ask_user, activate_tools) can emit out-of-band.
    let tool_extensions = || {
        let mut ext = params.extensions.clone();
        ext.insert(Arc::new(events.clone()));
        ext
    };

    events.emit(frame, parent, LoopEvent::TurnStarted);

    // First selection of the turn.
    let mut handle: ModelHandle = match deps.models.select(&params.model_hint, &[]).await {
        Ok(h) => h,
        Err(e) => {
            events.emit(frame, parent, LoopEvent::Error(format!("model selection failed: {e}")));
            return Err(e);
        }
    };

    for round in 0..deps.max_rounds {
        if token.is_cancelled() {
            return finish(TurnOutcome::Cancelled, &deps, &hook_ctx(), &events, frame, parent).await;
        }
        for h in &deps.hooks {
            h.before_round(round, &hook_ctx()).await;
        }
        events.emit(frame, parent, LoopEvent::RoundStarted { round });

        // Live input (pull-based, blueprint D10): user messages queued mid-turn.
        if let Some(input) = &params.live_input {
            for msg in input.drain().await {
                let id = store.append(frame, msg.clone()).await?;
                events.emit(frame, parent, LoopEvent::UserMessage {
                    message_id: id,
                    content: msg.content,
                    synthetic: msg.synthetic,
                    metadata: msg.metadata,
                });
            }
        }

        let turn_info = crate::context::TurnInfo {
            conversation: params.conversation.clone(),
            frame,
            agent: params.agent.clone(),
            user_message: params.meta.user_message.clone(),
        };

        let system = params.system.system_context(&turn_info).await?;
        let mut messages = assembler
            .build(&store, &AssembleInput {
                frame,
                system: system.clone(),
                model: handle.info.clone(),
                round,
            })
            .await?;
        let mut defs = params.tools.defs(&handle.info);

        // ── one LLM call with fallback ──
        let mut tried: Vec<ModelId> = vec![handle.id.clone()];
        let response: ModelResponse = loop {
            let (delta_tx, forwarder) = spawn_delta_forwarder(&events, frame, parent);
            let req = ModelRequest {
                messages: messages.clone(),
                tools: defs.clone(),
                model: handle.id.clone(),
                max_tokens: None,
                temperature: None,
                request_id: mint_request_id(),
                conversation: params.conversation.clone(),
                frame,
                extras: handle.info.extras.clone(),
                log: None,
            };
            let result = tokio::select! {
                biased;
                _ = token.cancelled() => {
                    drop(forwarder);
                    return finish(TurnOutcome::Cancelled, &deps, &hook_ctx(), &events, frame, parent).await;
                }
                r = handle.model.complete(&req, Some(delta_tx)) => r,
            };
            // Drain deltas BEFORE the round's outcome events (ordering).
            let _ = forwarder.await;

            match result {
                Ok(resp) => {
                    deps.models.report_success(&handle.id).await;
                    break resp;
                }
                Err(e) => {
                    deps.models.report_failure(&handle.id, &e.to_string()).await;
                    let retriable = handle.model.is_retriable(&e);
                    warn!(model = %handle.id, error = %e, retriable, "llm call failed");
                    if !retriable || tried.len() >= deps.retry.max_attempts {
                        events.emit(frame, parent, LoopEvent::LlmFailed {
                            tried: tried.clone(),
                            last_error: e.to_string(),
                        });
                        return Err(anyhow!("llm call failed on {}: {e}", handle.id));
                    }
                    match deps.models.select(&params.model_hint, &tried).await {
                        Ok(next) => {
                            events.emit(frame, parent, LoopEvent::ModelFallback {
                                from: handle.id.clone(),
                                to: next.id.clone(),
                                reason: e.to_string(),
                            });
                            handle = next;
                            tried.push(handle.id.clone());
                            // Rebuild for the new model: prompt_cache /
                            // capabilities / DTL mode may differ.
                            messages = assembler
                                .build(&store, &AssembleInput {
                                    frame,
                                    system: system.clone(),
                                    model: handle.info.clone(),
                                    round,
                                })
                                .await?;
                            defs = params.tools.defs(&handle.info);
                        }
                        Err(sel_err) => {
                            events.emit(frame, parent, LoopEvent::LlmFailed {
                                tried: tried.clone(),
                                last_error: format!("{e}; no fallback: {sel_err}"),
                            });
                            return Err(anyhow!("llm call failed on {} and no fallback: {e}", handle.id));
                        }
                    }
                }
            }
        };

        match response {
            ModelResponse::Message { content, reasoning, usage, .. } => {
                let id = store
                    .append(frame, NewMessage::assistant(content.clone(), reasoning.clone()))
                    .await?;
                store.set_usage(id, &usage).await?;
                if usage.truncated {
                    events.emit(frame, parent, LoopEvent::Truncated { output_tokens: usage.output_tokens });
                }
                events.emit(frame, parent, LoopEvent::Done {
                    message_id: id,
                    content: content.clone(),
                    usage: usage.clone(),
                    reasoning: reasoning.clone(),
                });
                let outcome = TurnOutcome::Final { content, message_id: id, usage, reasoning };
                return finish(outcome, &deps, &hook_ctx(), &events, frame, parent).await;
            }
            ModelResponse::ToolCalls { content, calls, reasoning, usage, .. } => {
                let msg_id = store
                    .append(frame, NewMessage::assistant(content.clone(), reasoning.clone()))
                    .await?;
                store.set_usage(msg_id, &usage).await?;
                if !content.is_empty() || usage.is_present() {
                    events.emit(frame, parent, LoopEvent::Thinking {
                        message_id: msg_id,
                        content,
                        usage,
                        reasoning,
                    });
                }

                let fan_out =
                    calls.len() >= 2 && calls.iter().all(|c| {
                        params
                            .tools
                            .find(&c.name)
                            .is_some_and(|t| t.concurrency_safe(&c.arguments))
                    });

                if fan_out {
                    if let Some(outcome) = run_fan_out(
                        &deps, &params, &events, &token, msg_id, &calls, tool_extensions(),
                    )
                    .await?
                    {
                        return finish(outcome, &deps, &hook_ctx(), &events, frame, parent).await;
                    }
                } else if let Some(outcome) = run_sequential(
                    &deps, &params, &events, &token, msg_id, &calls, tool_extensions(),
                )
                .await?
                {
                    return finish(outcome, &deps, &hook_ctx(), &events, frame, parent).await;
                }
            }
        }

        for h in &deps.hooks {
            h.after_round(round, &hook_ctx()).await;
        }
    }

    finish(TurnOutcome::Exhausted, &deps, &hook_ctx(), &events, frame, parent).await
}

/// Terminal helper: hooks.on_turn_end (+ Cancelled event) then return.
async fn finish(
    outcome: TurnOutcome,
    deps:    &Arc<KernelDeps>,
    ctx:     &HookCtx,
    events:  &EventSink,
    frame:   FrameId,
    parent:  Option<FrameId>,
) -> crate::Result<TurnOutcome> {
    if matches!(outcome, TurnOutcome::Cancelled) {
        events.emit(frame, parent, LoopEvent::Cancelled);
    }
    for h in &deps.hooks {
        h.on_turn_end(&outcome, ctx).await;
    }
    Ok(outcome)
}

/// Map streamed deltas to bus events; drained before the round's outcomes.
fn spawn_delta_forwarder(
    events: &EventSink,
    frame: FrameId,
    parent: Option<FrameId>,
) -> (mpsc::Sender<StreamDelta>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<StreamDelta>(256);
    let events = events.clone();
    let handle = tokio::spawn(async move {
        while let Some(delta) = rx.recv().await {
            let (kind, text) = match delta {
                StreamDelta::Text(t)      => (crate::events::DeltaKind::Content, t),
                StreamDelta::Reasoning(t) => (crate::events::DeltaKind::Reasoning, t),
            };
            events.emit(frame, parent, LoopEvent::TokenDelta { kind, text });
        }
    });
    (tx, handle)
}

/// Sequential tool-call path (a lone call, or any mixed batch). Returns
/// `Ok(Some(outcome))` when the turn must end (cancel/suspend).
async fn run_sequential(
    deps:    &Arc<KernelDeps>,
    params:  &LoopParams,
    events:  &EventSink,
    token:   &CancellationToken,
    msg_id:  MessageId,
    calls:   &[crate::model::ToolCall],
    ext:     crate::tool::Extensions,
) -> crate::Result<Option<TurnOutcome>> {
    let store = deps.store.clone();
    for call in calls {
        if token.is_cancelled() {
            return Ok(Some(TurnOutcome::Cancelled));
        }
        let ptc = record_call(&store, events, params, msg_id, call).await?;

        let pre = pre_execution(deps, params, events, token, &ptc).await?;
        let tool = match pre {
            PreExecution::Run(tool) => tool,
            PreExecution::Resolved(outcome) => {
                record_outcome(deps, params, events, &store, &ptc, outcome).await?;
                continue;
            }
            PreExecution::TurnCancelled => return Ok(Some(TurnOutcome::Cancelled)),
            PreExecution::Suspended => return Ok(Some(TurnOutcome::Cancelled)),
        };

        let ctx = ToolCtx {
            conversation: params.conversation.clone(),
            frame: params.frame,
            agent: params.agent.clone(),
            call_id: ptc.id,
            cancel: token.clone(),
            extensions: ext.clone(),
        };
        let exec = tool.start(ptc.arguments.clone(), &ctx);
        match drive_execution(&*exec, token).await {
            ExecutionOutcome::Suspended => {
                // The call STAYS AwaitingHuman (the tool marked it) — no resolve.
                return Ok(Some(TurnOutcome::Cancelled));
            }
            outcome => {
                record_outcome(deps, params, events, &store, &ptc, outcome.into_call_outcome())
                    .await?;
            }
        }
    }
    Ok(None)
}

/// The concurrent fan-out (generalized sub-agent batch, blueprint §5): ids
/// allocated in order (phase 1), execution concurrent and bounded (phase 2),
/// recording in order (phase 3).
async fn run_fan_out(
    deps:    &Arc<KernelDeps>,
    params:  &LoopParams,
    events:  &EventSink,
    token:   &CancellationToken,
    msg_id:  MessageId,
    calls:   &[crate::model::ToolCall],
    ext:     crate::tool::Extensions,
) -> crate::Result<Option<TurnOutcome>> {
    let store = deps.store.clone();

    // ── Phase 1: sequential, in call order ──
    let mut ptcs = Vec::with_capacity(calls.len());
    for call in calls {
        ptcs.push(record_call(&store, events, params, msg_id, call).await?);
    }

    // ── Phase 2: concurrent, bounded ──
    let futs: Vec<_> = ptcs
        .iter()
        .enumerate()
        .map(|(idx, ptc)| phase2_one(deps, params, events, token.clone(), ext.clone(), idx, ptc))
        .collect();
    let results: HashMap<usize, Phase2> = futures::stream::iter(futs)
        .buffer_unordered(deps.max_parallel_calls.max(1))
        .collect()
        .await;

    // ── Phase 3: sequential, in call order ──
    let mut suspended = false;
    for (idx, ptc) in ptcs.iter().enumerate() {
        match results.get(&idx) {
            Some(Phase2::Suspended) => {
                // Stays AwaitingHuman; the turn ends after recording the rest.
                suspended = true;
            }
            Some(Phase2::Done(outcome)) => {
                record_outcome(deps, params, events, &store, ptc, outcome.clone()).await?;
            }
            None => {
                record_outcome(
                    deps, params, events, &store, ptc,
                    CallOutcome::Failed("internal: fan-out result missing".into()),
                )
                .await?;
            }
        }
    }

    if suspended {
        return Ok(Some(TurnOutcome::Cancelled));
    }
    if token.is_cancelled() {
        return Ok(Some(TurnOutcome::Cancelled));
    }
    Ok(None)
}

enum Phase2 {
    Done(CallOutcome),
    Suspended,
}

/// One fanned-out call: gate → hooks.pre → execute. An explicit async fn (not
/// a closure) so the futures are uniform and the borrows are higher-ranked.
async fn phase2_one<'a>(
    deps:   &'a Arc<KernelDeps>,
    params: &'a LoopParams,
    events: &'a EventSink,
    token:  CancellationToken,
    ext:    crate::tool::Extensions,
    idx:    usize,
    ptc:    &'a PendingToolCall,
) -> (usize, Phase2) {
    let phase = match pre_execution(deps, params, events, &token, ptc).await {
        Ok(PreExecution::Run(tool)) => {
            let ctx = ToolCtx {
                conversation: params.conversation.clone(),
                frame: params.frame,
                agent: params.agent.clone(),
                call_id: ptc.id,
                cancel: token.clone(),
                extensions: ext,
            };
            let exec = tool.start(ptc.arguments.clone(), &ctx);
            match drive_execution(&*exec, &token).await {
                ExecutionOutcome::Suspended => Phase2::Suspended,
                outcome => Phase2::Done(outcome.into_call_outcome()),
            }
        }
        Ok(PreExecution::Resolved(outcome)) => Phase2::Done(outcome),
        Ok(PreExecution::TurnCancelled) => Phase2::Done(CallOutcome::Cancelled),
        Ok(PreExecution::Suspended) => Phase2::Suspended,
        Err(e) => Phase2::Done(CallOutcome::Failed(format!("pre-execution error: {e}"))),
    };
    (idx, phase)
}

/// Phase-1 shared by both paths: allocate the id and emit `ToolCallStarted`.
async fn record_call(
    store:  &Arc<dyn HistoryStore>,
    events: &EventSink,
    params: &LoopParams,
    msg_id: MessageId,
    call:   &crate::model::ToolCall,
) -> crate::Result<PendingToolCall> {
    let id = store
        .append_call(msg_id, NewCall {
            provider_id: if call.id.is_empty() { None } else { Some(call.id.clone()) },
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        })
        .await?;
    events.emit(params.frame, params.parent_frame, LoopEvent::ToolCallStarted {
        id,
        message_id: msg_id,
        name: call.name.clone(),
        args: call.arguments.clone(),
    });
    Ok(PendingToolCall {
        id,
        message_id: msg_id,
        provider_id: Some(call.id.clone()).filter(|s| !s.is_empty()),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
    })
}

enum PreExecution {
    Run(Arc<dyn crate::tool::Tool>),
    Resolved(CallOutcome),
    TurnCancelled,
    /// The gate suspended awaiting a human: the call STAYS `AwaitingHuman`
    /// (never resolved) and the turn ends.
    Suspended,
}

/// Gate + hooks.pre + tool lookup — shared by sequential and fan-out paths.
async fn pre_execution(
    deps:   &Arc<KernelDeps>,
    params: &LoopParams,
    events: &EventSink,
    token:  &CancellationToken,
    ptc:    &PendingToolCall,
) -> crate::Result<PreExecution> {
    let pending = PendingCall {
        id: ptc.id,
        name: ptc.name.clone(),
        args: ptc.arguments.clone(),
        frame: params.frame,
        agent: params.agent.clone(),
        extensions: params.extensions.clone(),
    };
    let decision = tokio::select! {
        biased;
        _ = token.cancelled() => return Ok(PreExecution::TurnCancelled),
        d = deps.gate.check(&pending, events) => d,
    };
    match decision {
        GateDecision::Reject { reason } => {
            return Ok(PreExecution::Resolved(CallOutcome::Rejected { reason }));
        }
        GateDecision::Suspend => return Ok(PreExecution::Suspended),
        GateDecision::Allow => {}
    }

    let mut ptc_mut = ptc.clone();
    let hook_ctx = HookCtx {
        conversation: params.conversation.clone(),
        frame: params.frame,
        agent: params.agent.clone(),
        store: deps.store.clone(),
        events: events.clone(),
    };
    for h in &deps.hooks {
        if let HookVerdict::Reject { reason } = h.pre_tool_call(&mut ptc_mut, &hook_ctx).await {
            return Ok(PreExecution::Resolved(CallOutcome::Rejected { reason }));
        }
    }

    match params.tools.find(&ptc.name) {
        Some(tool) => Ok(PreExecution::Run(tool)),
        None => Ok(PreExecution::Resolved(CallOutcome::Failed(format!(
            "unknown tool '{}' (not in this turn's tool set)",
            ptc.name
        )))),
    }
}

/// Phase-3 shared by both paths: hooks.post → resolve → emit.
async fn record_outcome(
    deps:   &Arc<KernelDeps>,
    params: &LoopParams,
    events: &EventSink,
    store:  &Arc<dyn HistoryStore>,
    ptc:    &PendingToolCall,
    outcome: CallOutcome,
) -> crate::Result<()> {
    let hook_ctx = HookCtx {
        conversation: params.conversation.clone(),
        frame: params.frame,
        agent: params.agent.clone(),
        store: store.clone(),
        events: events.clone(),
    };
    for h in &deps.hooks {
        h.post_tool_call(ptc, &outcome, &hook_ctx).await;
    }
    store.resolve_call(ptc.id, &outcome).await?;
    events.emit(params.frame, params.parent_frame, LoopEvent::ToolCallFinished {
        id: ptc.id,
        outcome,
    });
    Ok(())
}
