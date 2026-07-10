use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::core::db::{chat_history, chat_llm_tools, chat_sessions_stack};
use crate::core::events::ServerEvent;
use crate::core::tools::{ToolDescriptionLength, ToolResult, tool_names as tn};

use super::{ChatSessionHandler, TurnOutcome};
use super::emitter::TurnEmitter;
use super::gate::GateOutcome;
use super::outcome::RecordFlow;
use super::interface_tools::{AgentRunConfig, InterfaceTool};

impl ChatSessionHandler {
    /// Dispatches a single tool call by name+args without going through the LLM loop.
    /// Used by the REST `resolve` endpoint and by `resume_pending_tools`.
    /// Does NOT update the DB — caller is responsible for `complete` / `fail`.
    pub async fn execute_tool(&self, name: &str, args: Value) -> anyhow::Result<ToolResult> {
        if let Some((srv, mcp_tool)) = crate::core::mcp::parse_mcp_tool_name(name) {
            return self.mcp.call(srv, mcp_tool, args).await;
        }
        self.tools.dispatch(name, args).await.map(ToolResult::Text)
    }

    /// Resumes the LLM loop for the current session WITHOUT appending a new user message.
    /// Intended for use after pending tool calls have been resolved externally
    /// (e.g. via the REST approve endpoint) so the LLM can produce a final response
    /// or make further tool calls using the now-complete history.
    pub async fn resume_turn(
        &self,
        client_name:          Option<String>,
        extra_system_context: Option<String>,
        interface_tools:      Vec<InterfaceTool>,
        tx:                   mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<()> {
        let _guard = self.processing.lock().await;
        // A resume is a fresh unit of work (async result injection, app-restart
        // recovery, WS resume): mint a new token so it does not inherit a stale
        // cancellation, while a /stop *during* the resume still cancels this token.
        let token = CancellationToken::new();
        *self.current_cancel.lock().unwrap() = token.clone();

        let pool = &self.db;
        let em   = TurnEmitter::new(&tx);
        let mut config = self.build_agent_config(
            client_name, extra_system_context, None, interface_tools, std::collections::HashMap::new(),
        ).await?;
        config.tail_reminder = None;

        // Prune any interrupted parallel sub-agent batch before the linear cascade,
        // which assumes a single active frame per depth (see method doc).
        self.reap_interrupted_parallel_batches().await?;

        let stack = match chat_sessions_stack::active_for_session(pool, self.session_id).await? {
            Some(s) => s,
            None    => {
                warn!(session_id = self.session_id, "resume_turn: no active stack, nothing to resume");
                return Ok(());
            }
        };

        info!(session_id = self.session_id, stack_id = stack.id, depth = stack.depth, "resume_turn start");

        // Resume pending/interrupted tools before running the LLM loop.
        let had_pending = self.resume_pending_tools(stack.id, &config, &token, &tx).await?;

        // Seed the cascade. Normally we (re)run the deepest active frame's LLM loop
        // (live injection only applies to a fresh interactive turn from handle_message).
        // Two special cases when nothing was pending AND the frame's last message is a
        // pure-text assistant reply (its own turn is already complete):
        //   • root frame (no parent)  → nothing to do, skip the LLM.
        //   • child frame (has parent) → its result was produced but never propagated
        //     (e.g. the turn task died right after the child finished). Seed the cascade
        //     from the existing final message — without re-running the LLM — so the
        //     parent's tool call is completed and the parent continues. Skipping here
        //     (as the old guard did unconditionally) left the parent wedged forever.
        let (mut current_outcome, mut current_stack) = 'seed: {
            if !had_pending {
                if let Some(msg) = chat_history::last_message_for_stack(pool, stack.id).await? {
                    if matches!(msg.role, chat_history::Role::Assistant)
                        && chat_llm_tools::for_message(pool, msg.id).await?.is_empty()
                    {
                        if stack.parent_tool_call_id.is_none() {
                            info!(session_id = self.session_id, stack_id = stack.id, "resume_turn: last message is pure-text assistant, turn already complete — skipping LLM");
                            return Ok(());
                        }
                        info!(session_id = self.session_id, stack_id = stack.id, "resume_turn: deepest frame is a completed child — cascading its existing result to the parent");
                        let outcome = TurnOutcome::Final {
                            content:       msg.content,
                            message_id:    msg.id,
                            input_tokens:  None,
                            output_tokens: None,
                            truncated:     false,
                            tool_calls:    Vec::new(),
                        };
                        break 'seed (outcome, stack);
                    }
                }
            }
            (self.run_agent_turn(stack.id, &config, &token, &tx, None).await?, stack)
        };

        // Cascade completion upward through parent stacks (handles app-restart recovery
        // when a sub-agent was running — child completes, then parent continues).
        loop {
            let Some(parent_tool_call_id) = current_stack.parent_tool_call_id else { break };

            // Determine the result string to propagate to the parent's call_agent tool.
            let (result_str, is_error) = match &current_outcome {
                TurnOutcome::Final { content, .. } => (content.clone(), false),
                TurnOutcome::Cancelled  => (format!("Sub-agent `{}` was cancelled.", current_stack.agent_id), true),
                TurnOutcome::Exhausted  => (format!("Sub-agent `{}` exhausted tool-call rounds.", current_stack.agent_id), true),
            };
            let result_preview = super::preview_truncate(&result_str, 500);

            // Complete or fail the parent's call_agent tool call.
            if is_error {
                chat_llm_tools::fail(pool, parent_tool_call_id, &result_str).await?;
            } else {
                chat_llm_tools::complete(pool, parent_tool_call_id, &result_str, "string").await?;
            }

            // Terminate the child stack so active_for_session() returns the parent next.
            let _ = chat_sessions_stack::terminate(pool, current_stack.id).await;

            // Emit events to the frontend.
            if is_error {
                em.tool_error(parent_tool_call_id, result_str).await;
            } else {
                em.tool_done(parent_tool_call_id, result_str, "string".to_string()).await;
            }

            // Now the parent is the deepest active stack.
            let parent_stack = match chat_sessions_stack::active_for_session(pool, self.session_id).await? {
                Some(s) => s,
                None    => {
                    warn!(session_id = self.session_id, "resume_turn cascade: no active stack after child terminated");
                    break;
                }
            };

            em.agent_done(
                current_stack.id,
                current_stack.agent_id.clone(),
                parent_stack.agent_id.clone(),
                result_preview,
            ).await;

            info!(
                session_id = self.session_id,
                child_stack  = current_stack.id,
                parent_stack = parent_stack.id,
                depth        = parent_stack.depth,
                "resume_turn: cascading to parent stack"
            );

            self.resume_pending_tools(parent_stack.id, &config, &token, &tx).await?;
            current_outcome = self.run_agent_turn(parent_stack.id, &config, &token, &tx, None).await?;
            current_stack = parent_stack;

        }

        // current_stack is now the root (depth=0); emit the final event.
        match current_outcome {
            TurnOutcome::Final { content, message_id, input_tokens, output_tokens, truncated, .. } => {
                info!(session_id = self.session_id, "resume_turn done");
                if truncated {
                    warn!(session_id = self.session_id, "response truncated");
                    em.truncated(output_tokens).await;
                }
                em.done(message_id, current_stack.id, content, input_tokens, output_tokens).await;
            }
            TurnOutcome::Cancelled => {
                info!(session_id = self.session_id, "resume_turn cancelled");
                em.error("Cancelled by user.".to_string()).await;
            }
            TurnOutcome::Exhausted => {
                error!(session_id = self.session_id, "resume_turn exhausted tool rounds");
                em.error("Exceeded tool-call rounds without a final answer.".to_string()).await;
            }
        }
        Ok(())
    }

    /// Restart recovery for an interrupted **parallel** sub-agent batch.
    ///
    /// A purely linear stack has at most one active frame per depth. Two or more
    /// active frames at the same depth can only mean a concurrent sub-agent batch
    /// (`handle_sub_agent_batch`) was in flight when the process died. This app is
    /// single-user and deliberately tolerates losing mid-turn work on restart, so
    /// rather than a complex multi-sibling re-drive we simply prune the batch:
    /// terminate every active frame from the shallowest multi-frame depth downward
    /// and fail the sub-agent tool call that spawned each. The parent frame is then
    /// left with a clean, fully-resolved set of tool calls and the normal linear
    /// cascade resumes it. A single interrupted sub-agent (one frame at its depth)
    /// is untouched and still recovers via the existing cascade.
    async fn reap_interrupted_parallel_batches(&self) -> anyhow::Result<()> {
        let pool   = &self.db;
        let active = chat_sessions_stack::active_all_for_session(pool, self.session_id).await?;

        let Some(d_min) = shallowest_parallel_depth(&active) else {
            return Ok(()); // linear stack — nothing to reap
        };

        warn!(
            session_id = self.session_id, depth = d_min,
            "restart recovery: pruning interrupted parallel sub-agent batch"
        );

        for frame in active.iter().filter(|f| f.depth >= d_min) {
            if let Some(parent_tool_call_id) = frame.parent_tool_call_id {
                let _ = chat_llm_tools::fail(
                    pool, parent_tool_call_id, "Sub-agent interrupted by restart (parallel batch).",
                ).await;
            }
            let _ = chat_sessions_stack::terminate(pool, frame.id).await;
        }
        Ok(())
    }

    /// Called at the start of `handle_message` (and by the REST endpoint after a manual
    /// resolve). Finds any `pending` tool calls left from a previous interrupted session,
    /// re-runs them through the approval gate, executes approved ones, and fails rejected
    /// or denied ones — so `run_agent_turn` sees complete history and can continue cleanly.
    pub async fn resume_pending_tools(
        &self,
        stack_id: i64,
        config:   &AgentRunConfig,
        token:    &CancellationToken,
        tx:       &mpsc::Sender<ServerEvent>,
    ) -> anyhow::Result<bool> {
        let pool    = &self.db;
        let em      = TurnEmitter::new(tx);
        let pending = chat_llm_tools::pending_for_stack(pool, stack_id).await?;
        if pending.is_empty() {
            return Ok(false);
        }

        info!(
            session_id = self.session_id, stack_id,
            count = pending.len(), "resuming pending tool calls"
        );

        for tc in pending {
            let args: Value = tc.arguments.as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Object(Default::default()));

            // A pending `execute_task` (mode=sync) or `execute_subtask` means a
            // sub-agent stack was active. The cascade in resume_turn() handles it
            // by running the child stack to completion and propagating the result
            // up — skip it here.
            if tc.name == tn::EXECUTE_TASK || tc.name == tn::EXECUTE_SUBTASK {
                info!(session_id = self.session_id, tool_call_id = tc.id, "resume: skipping sub-agent dispatch (handled by stack cascade)");
                continue;
            }

            // `ask_user_clarification` is a synthetic tool (not in the registry).
            // Re-dispatch it directly so the question is re-asked to the user.
            if tc.name == tn::ASK_USER_CLARIFICATION {
                info!(session_id = self.session_id, tool_call_id = tc.id, "resume: re-asking clarification question");
                em.tool_start(
                    tc.id,
                    tc.message_id,
                    tc.name.clone(),
                    args.clone(),
                    self.tools.describe_call(&tc.name, &args, ToolDescriptionLength::Short),
                    self.tools.describe_call(&tc.name, &args, ToolDescriptionLength::Full),
                    self.tools.target_path(&tc.name, &args),
                ).await;
                let result = self.dispatch_ask_user_clarification(tc.id, &args, tx).await;
                match result {
                    Ok(answer) => {
                        chat_llm_tools::complete(pool, tc.id, &answer, "string").await?;
                        em.tool_done(tc.id, answer, "string".to_string()).await;
                    }
                    Err(e) if matches!(e.downcast_ref::<super::AgentFlowSignal>(), Some(super::AgentFlowSignal::QuestionChannelClosed)) => {
                        // WS disconnected again mid-resume. Tool stays 'pending' — next resume re-asks.
                        warn!(session_id = self.session_id, tool_call_id = tc.id, "clarification channel closed during resume — aborting");
                        return Ok(true);
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        chat_llm_tools::fail(pool, tc.id, &msg).await?;
                        em.tool_error(tc.id, msg).await;
                    }
                }
                continue;
            }

            // Announce the tool is being re-tried.
            em.tool_start(
                tc.id,
                tc.message_id,
                tc.name.clone(),
                args.clone(),
                self.tools.describe_call(&tc.name, &args, ToolDescriptionLength::Short),
                self.tools.describe_call(&tc.name, &args, ToolDescriptionLength::Full),
                self.tools.target_path(&tc.name, &args),
            ).await;

            // Re-run through the same approval gate as a live turn (current rules,
            // RunContext fast-path, auto-deny). Deny/reject paths mark the DB row and
            // emit the event internally; a closed channel leaves the tool pending.
            match self.run_approval_gate(tc.id, &tc.name, &args, &config.agent_id, &em).await? {
                GateOutcome::Proceed       => {}
                GateOutcome::Rejected      => continue,
                GateOutcome::ChannelClosed => return Ok(true), // pending still, WS disconnected
            }

            // `restart` calls process::exit and never returns — mark done first.
            if tc.name == tn::RESTART {
                info!(session_id = self.session_id, tool_call_id = tc.id, "restart approved (resume) — marking done then exiting");
                chat_llm_tools::complete(pool, tc.id, "Riavvio avviato.", "string").await?;
                em.tool_done(tc.id, "Riavvio avviato.".to_string(), "string".to_string()).await;
                // Use _exit() to skip C atexit handlers (e.g. Metal GPU cleanup in
                // whisper-rs/ggml, which aborts with SIGABRT and yields exit code 134
                // instead of 255 — breaking the run.sh restart supervisor).
                unsafe { libc::_exit(-1) }
            }

            // Re-run the persisted intent through the SAME dispatcher as a live turn
            // (`execute_tool_call`), not the flat `build_execution`. This routes
            // sub-agent tools (`execute_task` mode=sync, `execute_subtask`,
            // `run_subtask`) through the recursive interception in `dispatch.rs`;
            // `build_execution` alone does not know them and would fail with
            // "Unknown tool: execute_task". Apply the RunContext working dir exactly
            // like the live loop.
            let effective_args = self.effective_args(&tc.name, &args).await;
            let outcome = match self.execute_tool_call(
                stack_id, config, tc.id, &tc.name, &effective_args, token, tx,
            ).await {
                super::dispatch::DispatchResult::Outcome(o) => o,
                // Clarification WS channel closed mid-resume — leave the tool pending
                // so the next resume re-asks (mirrors the live turn's AbortPending).
                super::dispatch::DispatchResult::AbortPending => return Ok(true),
            };
            // resume passes `None`: it does not accumulate ToolCallEvents nor re-emit
            // FileChanged (only a live turn does). A /stop mid-resume returns Abort.
            match self.record_tool_outcome(tc.id, &tc.name, &effective_args, outcome, &em, None).await? {
                RecordFlow::Continue => {}
                RecordFlow::Abort    => return Ok(true),
            }
        }

        Ok(true)
    }
}

/// Shallowest stack depth that has more than one active (non-terminated) frame —
/// the top of an interrupted parallel sub-agent batch. Returns `None` for a linear
/// stack, where every depth has at most one active frame. Pure (see tests).
fn shallowest_parallel_depth(active: &[chat_sessions_stack::SessionStack]) -> Option<i64> {
    let mut by_depth: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for f in active {
        *by_depth.entry(f.depth).or_default() += 1;
    }
    by_depth.iter()
        .filter_map(|(depth, count)| (*count > 1).then_some(*depth))
        .min()
}

#[cfg(test)]
mod tests {
    use super::shallowest_parallel_depth;
    use crate::core::db::chat_sessions_stack::SessionStack;

    fn frame(id: i64, depth: i64, parent: Option<i64>) -> SessionStack {
        SessionStack { id, agent_id: "agent".into(), depth, parent_tool_call_id: parent }
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
            frame(2, 1, Some(10)), frame(3, 1, Some(11)),
            frame(4, 2, Some(30)),
        ];
        assert_eq!(shallowest_parallel_depth(&frames), Some(1));
    }

    #[test]
    fn detects_deeper_batch_when_upper_levels_linear() {
        let frames = vec![
            frame(1, 0, None),
            frame(2, 1, Some(10)),
            frame(3, 2, Some(20)), frame(4, 2, Some(21)),
        ];
        assert_eq!(shallowest_parallel_depth(&frames), Some(2));
    }
}
