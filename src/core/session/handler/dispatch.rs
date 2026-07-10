//! Working-directory argument rewriting and the per-tool-call dispatch router.
//!
//! Extracted from `run_agent_turn`: `effective_args` applies the RunContext working
//! directory to a call's arguments, and `execute_tool_call` routes an approved call
//! to the right executor (special non-cancellable paths + the unified cancellable
//! `ToolExecution` path).

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::core::events::ServerEvent;
use crate::core::tools::{drive_execution, tool_names as tn, ExecutionOutcome, ToolResult};

use super::ChatSessionHandler;
use super::interface_tools::AgentRunConfig;

/// Whether a tool call is a synchronous sub-agent dispatch, i.e. one intercepted
/// by `execute_tool_call` and routed to `dispatch_sub_agent` rather than the
/// registry. Covers `execute_task` (mode=sync), `execute_subtask`, and the legacy
/// `run_subtask` alias (only reachable via a `pending` call left across a restart).
/// Shared by the router below and the parallel-batch detection in `run_agent_turn`.
pub(super) fn is_sync_sub_agent(tool_name: &str, args: &Value) -> bool {
    (tool_name == tn::EXECUTE_TASK && args["mode"].as_str() == Some("sync") && args.get("agent_id").is_some())
        || tool_name == tn::EXECUTE_SUBTASK
        || tool_name == "run_subtask"
}

/// Result of routing a single tool call to its executor.
pub(super) enum DispatchResult {
    /// Normal completion / failure / cancellation — the caller records it.
    Outcome(ExecutionOutcome),
    /// The turn must end now and the tool row must stay `pending`: the
    /// `ask_user_clarification` WS channel closed while awaiting an answer. The
    /// caller returns `TurnOutcome::Cancelled` **without** recording the tool, so
    /// `resume_pending_tools` re-asks it on reconnect.
    AbortPending,
}

impl ChatSessionHandler {
    /// Applies the RunContext working directory to a tool call's arguments:
    /// resolves a relative `path` against the effective WD and injects `workdir`
    /// for `execute_cmd`. The caller keeps the original `arguments` for the
    /// `ToolStart` event / DB logging; this returns the copy used for execution.
    pub(super) async fn effective_args(&self, tool_name: &str, args: &Value) -> Value {
        let mut effective = args.clone();
        let wd = self.run_context.read().await
            .as_ref()
            .map(|rc| rc.effective_working_dir());
        if let Some(wd) = wd {
            if let Some(path) = effective["path"].as_str()
                && !std::path::Path::new(path).is_absolute()
            {
                effective["path"] = Value::String(wd.join(path).to_string_lossy().into_owned());
            }
            if tool_name == tn::EXECUTE_CMD && effective.get("workdir").is_none() {
                effective["workdir"] = Value::String(wd.to_string_lossy().into_owned());
            }
        }
        effective
    }

    /// Routes one already-approved tool call to the right executor. Covers the
    /// special, non-cancellable paths (sub-agent, scratchpad, todos, clarification,
    /// the `task_completed` stub) and the unified cancellable `ToolExecution` path
    /// (registry / memory / image / interface / MCP). `restart` is handled by the
    /// caller before this is reached (it calls `_exit` and never returns).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_tool_call(
        &self,
        stack_id:     i64,
        config:       &AgentRunConfig,
        tool_call_id: i64,
        tool_name:    &str,
        args:         &Value,
        token:        &CancellationToken,
        tx:           &mpsc::Sender<ServerEvent>,
    ) -> DispatchResult {
        let outcome: ExecutionOutcome = if is_sync_sub_agent(tool_name, args) {
            plain_outcome(self.dispatch_sub_agent(stack_id, config, tool_call_id, args, token, tx).await)
        } else if tool_name == tn::UPDATE_SCRATCHPAD {
            plain_outcome(self.dispatch_update_scratchpad(args).await)
        } else if tool_name == tn::WRITE_TODOS {
            plain_outcome(self.dispatch_write_todos(args).await)
        } else if tool_name == tn::ASK_USER_CLARIFICATION {
            match self.dispatch_ask_user_clarification(tool_call_id, args, tx).await {
                Ok(answer) => ExecutionOutcome::Completed(ToolResult::Text(answer)),
                Err(err) => {
                    // WS disconnected while waiting for a clarification answer.
                    // Tool stays 'pending' in DB — resume_pending_tools re-dispatches on reconnect.
                    if matches!(err.downcast_ref::<super::AgentFlowSignal>(), Some(super::AgentFlowSignal::QuestionChannelClosed)) {
                        warn!(session_id = self.session_id, tool_call_id, "clarification channel closed — aborting turn (tool stays pending)");
                        return DispatchResult::AbortPending;
                    }
                    ExecutionOutcome::Failed(err.to_string())
                }
            }
        } else if tool_name == "task_completed" {
            // Defensive stub: if the LLM somehow calls this itself, return a hint.
            // Real delivery is via inject_async_result (synthetic message from the system).
            let task_id = args["task_id"].as_i64().unwrap_or(0);
            ExecutionOutcome::Completed(ToolResult::Text(format!(r#"{{"status":"not_ready","task_id":{task_id},"message":"This tool is invoked by the system, not by you. Do not call it again — the result will arrive automatically as a new message in this conversation."}}"#)))
        } else {
            // Unified cancellable path. The execution owns its in-flight state and
            // its own stop(); on /stop the work future is dropped (aborting I/O /
            // killing the child) and the tool is recorded as Cancelled, not Failed.
            match self.build_execution(tool_name, args.clone(), config) {
                Some(exec) => drive_execution(exec.as_ref(), token).await,
                None        => ExecutionOutcome::Failed(format!("Unknown tool: {tool_name}")),
            }
        };
        DispatchResult::Outcome(outcome)
    }
}

/// Maps a plain dispatch `Result<String>` to an [`ExecutionOutcome`]. Used by the
/// non-cancellable special paths (sub-agent, scratchpad, todos), which can only
/// complete or fail — never `Cancelled`.
fn plain_outcome(result: anyhow::Result<String>) -> ExecutionOutcome {
    match result {
        Ok(s)  => ExecutionOutcome::Completed(ToolResult::Text(s)),
        Err(e) => ExecutionOutcome::Failed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::is_sync_sub_agent;
    use serde_json::json;

    #[test]
    fn recognises_sync_sub_agent_calls() {
        assert!(is_sync_sub_agent("execute_task", &json!({"mode": "sync", "agent_id": "x"})));
        assert!(is_sync_sub_agent("execute_subtask", &json!({})));
        assert!(is_sync_sub_agent("run_subtask", &json!({}))); // legacy alias
    }

    #[test]
    fn rejects_everything_else() {
        // execute_task without mode=sync + agent_id is NOT a sync sub-agent.
        assert!(!is_sync_sub_agent("execute_task", &json!({"mode": "async", "agent_id": "x"})));
        assert!(!is_sync_sub_agent("execute_task", &json!({"mode": "sync"})));   // no agent_id
        assert!(!is_sync_sub_agent("execute_task", &json!({})));
        // Regular tools never qualify (they must keep the sequential path).
        assert!(!is_sync_sub_agent("read_file", &json!({"path": "/x"})));
        assert!(!is_sync_sub_agent("execute_cmd", &json!({"cmd": "ls"})));
    }
}
