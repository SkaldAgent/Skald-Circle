use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, trace};

use crate::chat_event_bus::ToolCallEvent;
use crate::chatbot::{LlmTurn, ToolCall};
use crate::db::{chat_history, chat_llm_tools};
use crate::events::ServerEvent;
use crate::tools::{
    ExecutionOutcome, SimpleExecution, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
};
use futures::stream::{self, StreamExt};

use super::{ChatSessionHandler, PendingUserInput, TurnOutcome};
use super::dispatch::{is_sync_sub_agent, DispatchResult};
use super::emitter::TurnEmitter;
use super::gate::GateOutcome;
use super::llm_call::RoundLlm;
use super::outcome::RecordFlow;
use super::interface_tools::AgentRunConfig;

/// Whether, after handling one tool call, the round loop should continue to the
/// next call or the whole turn should end.
enum CallFlow {
    Continue,
    End(TurnOutcome),
}

/// Outcome of gating + dispatching one call inside a concurrent sub-agent batch,
/// carried from the concurrent phase to the ordered recording phase.
enum GatedExec {
    /// Gate passed; the sub-agent produced an outcome to record. `arguments` is
    /// the call's args (used for FileChanged / logging).
    Done { arguments: serde_json::Value, outcome: ExecutionOutcome },
    /// Approval gate rejected the call — already marked/emitted by the gate; skip it.
    Rejected,
    /// The turn must end now: the clarification WS channel closed (dispatch returned
    /// `AbortPending`) or the approval gate's channel closed.
    AbortTurn,
}

impl ChatSessionHandler {
    /// Inner loop of an agent (root or sub). Persists messages to `stack_id`,
    /// emits Thinking/ToolStart/ToolDone/PendingWrite/ApprovalRequired/AgentStart/AgentDone events.
    /// Returns the outcome; the caller decides what to emit on completion
    /// (Done for root, AgentDone+tool-result for sub-agents).
    pub(super) fn run_agent_turn<'a>(
        &'a self,
        stack_id: i64,
        config:   &'a AgentRunConfig,
        token:    &'a CancellationToken,
        tx:       &'a mpsc::Sender<ServerEvent>,
        // Queued user input for live injection (root interactive turn only).
        // `None` for sub-agents / resume / non-interactive runners.
        pending_input: Option<&'a Arc<dyn PendingUserInput>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<TurnOutcome>> + Send + 'a>> {
        Box::pin(async move {
        let pool = &self.db;
        let em   = TurnEmitter::new(tx);

        // Resolve the initial model. `cur_name`/`cur_llm` are updated in-place
        // when the fallback logic switches to a different model mid-turn.
        let mut cur_name = config.client_name.clone();
        let mut cur_llm  = self.llm_manager.get(&cur_name).await
            .ok_or_else(|| anyhow::anyhow!("LLM client '{}' not found", cur_name))?;

        // Scope/strength needed for fallback re-selection.
        let meta         = crate::agents::load_meta(&config.agent_id).ok();
        let req_scope    = meta.as_ref().and_then(|m| m.scope.as_deref()).map(str::to_string);
        let req_strength = meta.as_ref().and_then(|m| m.strength);

        // Accumulates tool calls across all rounds for the event bus.
        let mut all_tool_calls: Vec<ToolCallEvent> = Vec::new();

        for round in 0..self.max_tool_rounds {
            if token.is_cancelled() {
                return Ok(TurnOutcome::Cancelled);
            }

            // ── Live user-message injection ─────────────────────────────────────
            // A round boundary is the one clean ordering point: the previous
            // round's assistant message + tool results are all persisted, so a
            // `user` row appended here is well-ordered. Each queued message is
            // saved individually and echoed (telnet-style: the bubble appears only
            // now), then picked up by `build_openai_messages` below in this same
            // round — so the model sees it immediately. The MessageBuilder merges
            // consecutive user rows into one `role:user` for the LLM. Does not
            // reset the round budget. Only ever `Some` for the root interactive turn.
            if let Some(input) = pending_input {
                for msg in input.drain_user().await {
                    let attachments = msg.metadata.as_ref()
                        .map(|m| m.attachments.clone())
                        .unwrap_or_default();
                    // A custom slash command persists its expanded template (for LLM
                    // replay) but the bubble must show the typed command — emit the
                    // command's `display` form when present.
                    let echo = msg.metadata.as_ref()
                        .and_then(|m| m.command.as_ref())
                        .map(|c| c.display.clone())
                        .unwrap_or_else(|| msg.content.clone());
                    let id = chat_history::append_with_metadata(
                        pool, stack_id, &chat_history::Role::User,
                        &msg.content, false, None, msg.metadata.as_ref(),
                    ).await?;
                    em.user_message(id, echo, attachments).await;
                }
            }

            trace!(session_id = self.session_id, stack_id, agent_id = config.agent_id, round, "starting round");

            let active_grants_snapshot = config.active_mcp_grants
                .read()
                .map(|g| g.clone())
                .unwrap_or_default();

            // Messages are (re)built with the current model's prompt_cache flag.
            // On fallback within the same round `call_llm_round` rebuilds them again
            // if the replacement model has a different prompt_cache setting.
            let mut messages = self.build_openai_messages(pool, stack_id, &config.agent_id, config.extra_system.as_deref(), config.extra_system_dynamic.as_deref(), config.tail_reminder.as_deref(), &active_grants_snapshot, &config.system_substitutions, cur_llm.prompt_cache, &cur_llm.capabilities).await?;
            let tool_defs    = config.all_tool_defs();

            // Record every tool actually offered to the LLM so the Security-groups
            // UI can list/gate dynamically-injected tools. Cheap no-op once each
            // name is known; new names are persisted off the turn's critical path.
            self.tool_discovery.observe(&tool_defs);

            // One LLM call for this round, with automatic model fallback on
            // retriable errors. `cur_name`/`cur_llm`/`messages` are updated in place.
            let turn_result = match self.call_llm_round(
                stack_id, config, &active_grants_snapshot, &tool_defs,
                req_scope.as_deref(), req_strength,
                &mut cur_name, &mut cur_llm, &mut messages, token, &em,
            ).await {
                RoundLlm::Turn(t)   => t,
                RoundLlm::Cancelled => return Ok(TurnOutcome::Cancelled),
                RoundLlm::Failed(e) => return Err(e),
            };

            match turn_result {
                LlmTurn::Message(resp) => {
                    let message_id = chat_history::append(
                        pool, stack_id, &chat_history::Role::Assistant, &resp.content, false,
                        resp.reasoning_content.as_deref(),
                    ).await?;
                    if let (Some(i), Some(o)) = (resp.input_tokens, resp.output_tokens) {
                        chat_history::set_usage(pool, message_id, i, o, 0, resp.cost).await?;
                    }
                    return Ok(TurnOutcome::Final {
                        content:       resp.content,
                        message_id,
                        input_tokens:  resp.input_tokens,
                        output_tokens: resp.output_tokens,
                        truncated:     resp.truncated,
                        reasoning_content: resp.reasoning_content,
                        tool_calls:    all_tool_calls,
                    });
                }

                LlmTurn::ToolCalls { content: assistant_text, calls, input_tokens, output_tokens, reasoning_content, cost, .. } => {
                    let message_id = chat_history::append(
                        pool, stack_id, &chat_history::Role::Assistant, &assistant_text, false,
                        reasoning_content.as_deref(),
                    ).await?;
                    if let (Some(i), Some(o)) = (input_tokens, output_tokens) {
                        chat_history::set_usage(pool, message_id, i, o, 0, cost).await?;
                    }
                    if !assistant_text.trim().is_empty() || input_tokens.is_some() {
                        em.thinking(message_id, assistant_text, input_tokens, output_tokens, reasoning_content).await;
                    }

                    // A homogeneous batch of ≥2 synchronous sub-agent calls is fanned
                    // out concurrently (bounded by `max_parallel_subagents`). Any other
                    // shape — a single call, or a mix with regular tools — keeps the
                    // strictly sequential path, so tool ordering and side-effects are
                    // unchanged for everything except this well-defined case.
                    if calls.len() >= 2 && calls.iter().all(|c| is_sync_sub_agent(&c.name, &c.arguments)) {
                        match self.handle_sub_agent_batch(
                            stack_id, config, message_id, &calls, token, tx, &em, &mut all_tool_calls,
                        ).await? {
                            CallFlow::Continue => {}
                            CallFlow::End(outcome) => return Ok(outcome),
                        }
                    } else {
                        for call in &calls {
                            // Stop before each call so a /stop (or a cancelled sub-agent,
                            // which shares this token) aborts the rest of the round.
                            if token.is_cancelled() {
                                return Ok(TurnOutcome::Cancelled);
                            }
                            match self.handle_tool_call(
                                stack_id, config, message_id, call, token, tx, &em, &mut all_tool_calls,
                            ).await? {
                                CallFlow::Continue => {}
                                CallFlow::End(outcome) => return Ok(outcome),
                            }
                        }
                    }
                }
            }
        }

        Ok(TurnOutcome::Exhausted)
        }) // end Box::pin
    }

    /// Handles a single tool call within a round: persists the call row, emits
    /// `ToolStart`, resolves the working directory, runs the approval gate, handles
    /// `restart`, dispatches, and records the outcome. Returns [`CallFlow::Continue`]
    /// Card metadata (friendly display name + semantic icon key) for a tool call.
    /// Delegates to the registry seam [`ToolRegistry::display_meta`], then layers the
    /// MCP display-name override on for an `mcp__server__tool` name (manifest title >
    /// live MCP `title` > the prettified name the seam already produced). The single
    /// place the live loop resolves a card title, mirroring `describe_call`.
    pub(super) fn tool_ui_meta(&self, name: &str, args: &serde_json::Value) -> (String, String) {
        let mut meta = self.tools.display_meta(name, args);
        if let Some((server, tool)) = crate::mcp::parse_mcp_tool_name(name) {
            if let Some(friendly) = self.mcp.tool_display_name(server, tool) {
                meta.display_name = friendly;
            }
        }
        (meta.display_name, meta.icon)
    }

    /// to move on to the next call, or [`CallFlow::End`] to end the whole turn.
    #[allow(clippy::too_many_arguments)]
    async fn handle_tool_call(
        &self,
        stack_id:       i64,
        config:         &AgentRunConfig,
        message_id:     i64,
        call:           &ToolCall,
        token:          &CancellationToken,
        tx:             &mpsc::Sender<ServerEvent>,
        em:             &TurnEmitter<'_>,
        all_tool_calls: &mut Vec<ToolCallEvent>,
    ) -> anyhow::Result<CallFlow> {
        let pool = &self.db;

        let args_str = serde_json::to_string(&call.arguments)
            .unwrap_or_else(|_| "{}".to_string());
        let tool_call_id = chat_llm_tools::append(pool, message_id, &call.name, &args_str).await?;
        let (display_name, icon) = self.tool_ui_meta(&call.name, &call.arguments);
        em.tool_start(
            tool_call_id, message_id,
            call.name.clone(),
            call.arguments.clone(),
            display_name, icon,
            self.tools.describe_call(&call.name, &call.arguments, ToolDescriptionLength::Short),
            self.tools.describe_call(&call.name, &call.arguments, ToolDescriptionLength::Full),
            self.tools.target_path(&call.name, &call.arguments),
        ).await;

        // Tool calls receive their arguments unchanged — the session working
        // directory is always the user's home (`~`), and the agent references
        // project files via their absolute agent path. `call.arguments` is both
        // logged and executed.

        match self.run_approval_gate(tool_call_id, &call.name, &call.arguments, &config.agent_id, em).await? {
            GateOutcome::Proceed       => {}
            GateOutcome::Rejected      => return Ok(CallFlow::Continue),
            GateOutcome::ChannelClosed => return Ok(CallFlow::End(TurnOutcome::Cancelled)),
        }

        debug!(session_id = self.session_id, tool = %call.name, tool_call_id, "dispatching");

        // Route the approved call to its executor. `AbortPending` means the
        // clarification WS channel closed — end the turn and leave the tool
        // `pending` for resume to re-ask.
        let (outcome, preview) = match self.execute_tool_call(
            stack_id, config, tool_call_id, &call.name, &call.arguments, token, tx,
        ).await {
            DispatchResult::Outcome { outcome, preview } => (outcome, preview),
            DispatchResult::AbortPending => return Ok(CallFlow::End(TurnOutcome::Cancelled)),
        };

        match self.record_tool_outcome(
            tool_call_id, &call.name, &call.arguments, outcome, preview, em, Some(all_tool_calls),
        ).await? {
            RecordFlow::Continue => Ok(CallFlow::Continue),
            RecordFlow::Abort    => Ok(CallFlow::End(TurnOutcome::Cancelled)),
        }
    }

    /// Concurrent variant of the tool-call loop for a homogeneous batch of
    /// synchronous sub-agent calls (`execute_task` mode=sync / `execute_subtask`).
    /// Only called when every call in the round is such a sub-agent (see the
    /// dispatch in `run_agent_turn`), so `restart` and side-effecting tools can
    /// never appear here and the sequential path is left byte-for-byte intact.
    ///
    /// Ordering invariant: the LLM reconstructs tool results by autoincrement id
    /// (`chat_llm_tools ORDER BY id ASC`). **Phase 1** therefore allocates every
    /// call's row in `calls` order *before* any concurrent work, so completion
    /// order is irrelevant. **Phase 2** runs the approval gate + dispatch for all
    /// calls concurrently, bounded by `max_parallel_subagents`. **Phase 3** records
    /// the outcomes back in `calls` order, so `all_tool_calls` ordering and the
    /// shared-token cancellation semantics match the sequential path.
    #[allow(clippy::too_many_arguments)]
    async fn handle_sub_agent_batch(
        &self,
        stack_id:       i64,
        config:         &AgentRunConfig,
        message_id:     i64,
        calls:          &[ToolCall],
        token:          &CancellationToken,
        tx:             &mpsc::Sender<ServerEvent>,
        em:             &TurnEmitter<'_>,
        all_tool_calls: &mut Vec<ToolCallEvent>,
    ) -> anyhow::Result<CallFlow> {
        let pool = &self.db;

        // ── Phase 1: allocate tool_call_id rows in `calls` order ────────────────────
        // The id fixes the LLM-visible order regardless of which sub-agent finishes
        // first, so this pre-pass MUST stay sequential and precede the fan-out.
        let mut started: Vec<(&ToolCall, i64)> = Vec::with_capacity(calls.len());
        for call in calls {
            let args_str = serde_json::to_string(&call.arguments)
                .unwrap_or_else(|_| "{}".to_string());
            let tool_call_id = chat_llm_tools::append(pool, message_id, &call.name, &args_str).await?;
            let (display_name, icon) = self.tool_ui_meta(&call.name, &call.arguments);
            em.tool_start(
                tool_call_id, message_id,
                call.name.clone(),
                call.arguments.clone(),
                display_name, icon,
                self.tools.describe_call(&call.name, &call.arguments, ToolDescriptionLength::Short),
                self.tools.describe_call(&call.name, &call.arguments, ToolDescriptionLength::Full),
                self.tools.target_path(&call.name, &call.arguments),
            ).await;
            started.push((call, tool_call_id));
        }

        // ── Phase 2: gate + dispatch concurrently, bounded ──────────────────────────
        // Every future borrows `&self`/`config`/`token`/`tx`/`em` (all shared refs)
        // and writes only to its own distinct child stack + tool_call_id, so there is
        // no shared mutable state between siblings. Results are keyed back by index.
        let limit = self.max_parallel_subagents.max(1);
        let mut results: Vec<Option<GatedExec>> = (0..started.len()).map(|_| None).collect();
        // Feed the stream fully-owned items `(idx, tool_call_id, name, arguments)`.
        // Passing a borrowed `&ToolCall` as the closure input makes the returned async
        // block's lifetime higher-ranked ("FnOnce is not general enough"); owning the
        // per-call data means each future only borrows `self`/`config`/`token`/`tx`/`em`
        // from the enclosing scope, all at the single concrete turn lifetime.
        let jobs: Vec<(usize, i64, String, serde_json::Value)> = started.iter().enumerate()
            .map(|(idx, (call, id))| (idx, *id, call.name.clone(), call.arguments.clone()))
            .collect();
        {
            let mut stream = stream::iter(jobs)
                .map(|(idx, tool_call_id, name, arguments)| async move {
                    let gated = match self.run_approval_gate(
                        tool_call_id, &name, &arguments, &config.agent_id, em,
                    ).await {
                        Ok(GateOutcome::Proceed) => match self.execute_tool_call(
                            stack_id, config, tool_call_id, &name, &arguments, token, tx,
                        ).await {
                            // Sub-agent batches never carry a file-write preview.
                            DispatchResult::Outcome { outcome, .. } => Ok(GatedExec::Done { arguments, outcome }),
                            DispatchResult::AbortPending            => Ok(GatedExec::AbortTurn),
                        },
                        Ok(GateOutcome::Rejected)      => Ok(GatedExec::Rejected),
                        Ok(GateOutcome::ChannelClosed) => Ok(GatedExec::AbortTurn),
                        Err(e)                         => Err(e),
                    };
                    (idx, gated)
                })
                .buffer_unordered(limit);

            while let Some((idx, gated)) = stream.next().await {
                results[idx] = Some(gated?);
            }
        }

        // ── Phase 3: record outcomes in `calls` order ───────────────────────────────
        let mut abort = false;
        for (idx, (call, tool_call_id)) in started.iter().enumerate() {
            match results[idx].take().expect("every started sub-agent call produced a result") {
                // The gate already marked the row rejected and emitted the event.
                GatedExec::Rejected  => {}
                GatedExec::AbortTurn => abort = true,
                GatedExec::Done { arguments, outcome } => {
                    match self.record_tool_outcome(
                        *tool_call_id, &call.name, &arguments, outcome, None, em, Some(all_tool_calls),
                    ).await? {
                        RecordFlow::Continue => {}
                        RecordFlow::Abort    => abort = true,
                    }
                }
            }
        }

        // The shared token means a /stop (or a cancelled sibling) has already stopped
        // the others; ending the turn here mirrors the sequential path's early return.
        if abort || token.is_cancelled() {
            Ok(CallFlow::End(TurnOutcome::Cancelled))
        } else {
            Ok(CallFlow::Continue)
        }
    }

    /// Builds a [`ToolExecution`] for a single tool call, covering every tool that
    /// flows through the unified (cancellable) dispatch path: interface tools,
    /// memory/image tools, MCP tools, and the built-in registry (incl.
    /// `execute_cmd`). Returns `None` only for an unknown tool name. The handle
    /// borrows `self` and `config`, both of which outlive the turn.
    pub(super) fn build_execution<'a>(
        &'a self,
        name:   &str,
        args:   serde_json::Value,
        config: &'a AgentRunConfig,
    ) -> Option<Box<dyn ToolExecution + 'a>> {
        // Interface tools (closures injected per-interface, e.g. activate_tools).
        if let Some(tool) = config.interface_tools.iter().find(|t| t.name() == name) {
            let handler = std::sync::Arc::clone(&tool.handler);
            return Some(Box::new(SimpleExecution::new(
                Box::pin(async move { handler(args).await.map(ToolResult::Text) }),
            )));
        }
        // The ToolContext carries this session's id, owner user id and owner pool
        // so owner-bound tools (cron management, the Honcho memory peer) act on the
        // caller's own data. Built once and shared by memory tools and the registry.
        let ctx = ToolContext {
            session_id: self.session_id,
            user_id: self.user_id.clone(),
            pool: Arc::clone(&self.db),
            // Snapshot the fs cell for the duration of this tool call — a concurrent
            // shared-folder remount swaps the cell, the next call picks it up (§6).
            fs: self.fs.load(),
        };
        // Memory + image tools (registered ad-hoc on the config). Memory tools route
        // through `run_with` so the Honcho tools reach the caller's own peer.
        if let Some(tool) = config.memory_tools.iter().find(|t| t.name() == name) {
            return Some(tool.run_with(&ctx, args));
        }
        if let Some(tool) = config.image_tools.iter().find(|t| t.name() == name) {
            return Some(tool.run(args));
        }
        // MCP tools (`server::tool`). Clone the Arc so the work future is 'static.
        if let Some((srv, mcp_tool)) = crate::mcp::parse_mcp_tool_name(name) {
            let mcp      = std::sync::Arc::clone(&self.mcp);
            let srv      = srv.to_string();
            let mcp_tool = mcp_tool.to_string();
            let fut: std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<ToolResult>> + Send>> =
                Box::pin(async move { mcp.call(&srv, &mcp_tool, args).await });
            return Some(Box::new(SimpleExecution::new(fut)));
        }
        // Built-in registry tools (incl. execute_cmd, whose SimpleExecution kills
        // the child via kill_on_drop when the work future is dropped on /stop).
        self.tools.run(name, &ctx, args)
    }
}
