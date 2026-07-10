//! Shared approval gate for a single tool call.
//!
//! The decision + human-approval flow (approval-engine check, RunContext
//! fast-path, auto-deny, register + await) was duplicated in `run_agent_turn` and
//! `resume_pending_tools`, and had already drifted (only the live loop applied the
//! RunContext fast-path and the auto-deny short-circuit). `run_approval_gate` is the
//! single implementation both call, so the two paths gate identically.

use std::sync::atomic::Ordering;

use serde_json::Value;
use tracing::{info, warn};

use crate::approval::GateResult;
use crate::db::chat_llm_tools;
use crate::run_context::RunContext;
use crate::tools::{is_file_read_tool, is_file_write_tool};

use super::{ApprovalDecision, ChatSessionHandler};
use super::emitter::TurnEmitter;

/// Result of the approval gate for a single tool call.
pub(super) enum GateOutcome {
    /// The tool may execute.
    Proceed,
    /// Denied by policy, auto-denied, or rejected by a human. The DB row has been
    /// marked `rejected` and the `ToolRejected` event emitted — the caller just
    /// skips the call.
    Rejected,
    /// The approval channel closed (WS disconnected) while awaiting a decision.
    /// The caller must end the turn / resume.
    ChannelClosed,
}

impl ChatSessionHandler {
    /// Runs a tool call through the approval engine and, when human approval is
    /// required, registers the request, emits the approval event, and awaits the
    /// decision. Shared by `run_agent_turn` and `resume_pending_tools`.
    pub(super) async fn run_approval_gate(
        &self,
        tool_call_id: i64,
        tool_name:    &str,
        args:         &Value,
        agent_id:     &str,
        em:           &TurnEmitter<'_>,
    ) -> anyhow::Result<GateOutcome> {
        let pool     = &self.db;

        // Post-restart manual resolve: this exact tool_call was already approved by the
        // user via a resolve endpoint, which then triggered this resume. There is no
        // live oneshot to unblock, so skip re-gating (and re-prompting) and dispatch it.
        if self.pre_approved.lock().unwrap().remove(&tool_call_id) {
            info!(session_id = self.session_id, tool = %tool_name, tool_call_id, "approval: pre-approved (post-restart resolve) — skipping gate");
            return Ok(GateOutcome::Proceed);
        }

        let category = self.tools.category_of(tool_name);
        let group_id = self.tool_group_id().await;

        // The approval engine decides first: an explicit Deny/Allow rule always wins.
        let mut gate = self.approval.check(
            self.session_id, category,
            agent_id, &self.source, tool_name, args,
            group_id.as_deref(),
        ).await;

        // RunContext fast-path: relax `Require` to `Allow` for pre-authorized
        // filesystem paths. It never overrides a `Deny` (same semantics as session
        // bypass), so e.g. the `secrets/` deny rule holds even inside an auto-read
        // working directory.
        if matches!(gate, GateResult::Require) {
            let path  = args["path"].as_str().unwrap_or("");
            let guard = self.run_context.read().await;
            let dflt  = RunContext::default();
            let rc    = guard.as_ref().unwrap_or(&dflt);
            let pre_allowed = if is_file_read_tool(tool_name) {
                rc.is_read_allowed(path)
            } else if is_file_write_tool(tool_name) {
                rc.is_write_allowed(path)
            } else {
                false
            };
            if pre_allowed { gate = GateResult::Allow; }
        }

        match gate {
            GateResult::Allow => Ok(GateOutcome::Proceed),
            GateResult::Deny => {
                let msg = "Tool call denied by approval policy.".to_string();
                info!(session_id = self.session_id, tool = %tool_name, tool_call_id, "approval: denied");
                chat_llm_tools::reject(pool, tool_call_id, &msg).await?;
                em.tool_rejected(tool_call_id, msg).await;
                Ok(GateOutcome::Rejected)
            }
            GateResult::Require => {
                if self.auto_deny_approvals.load(Ordering::Relaxed) {
                    let msg = "Tool call auto-denied: this session does not support approval requests.".to_string();
                    info!(session_id = self.session_id, tool = %tool_name, tool_call_id, "auto_deny_approvals: denied");
                    chat_llm_tools::reject(pool, tool_call_id, &msg).await?;
                    em.tool_rejected(tool_call_id, msg).await;
                    return Ok(GateOutcome::Rejected);
                }

                // Mark as pending before suspending so restart/refresh shows the
                // approval form (not "Interrupted") and auto-resume re-gates.
                chat_llm_tools::set_approval_pending(pool, tool_call_id).await?;

                let ctx_label = self.context_label.read().ok().and_then(|g| g.clone());
                let (request_id, approve_rx) = self.approval.register(
                    self.session_id, tool_call_id, tool_name,
                    args.clone(), agent_id, &self.source,
                    ctx_label.as_deref(), category,
                ).await;
                info!(session_id = self.session_id, tool = %tool_name, tool_call_id, request_id, "approval: waiting for human");
                self.emit_approval_event(em, request_id, tool_call_id, tool_name, args).await;

                match approve_rx.await {
                    Ok(ApprovalDecision::Approved) => {
                        info!(session_id = self.session_id, request_id, tool = %tool_name, "approval: approved");
                        Ok(GateOutcome::Proceed)
                    }
                    Ok(ApprovalDecision::Rejected { note }) => {
                        info!(session_id = self.session_id, request_id, tool = %tool_name, %note, "approval: rejected");
                        let msg = ApprovalDecision::rejection_message(&note);
                        chat_llm_tools::reject(pool, tool_call_id, &msg).await?;
                        em.tool_rejected(tool_call_id, msg).await;
                        Ok(GateOutcome::Rejected)
                    }
                    Err(_) => {
                        // WS closed while waiting — session is orphaned.
                        warn!(session_id = self.session_id, request_id, "approval channel closed (WS disconnected), aborting");
                        Ok(GateOutcome::ChannelClosed)
                    }
                }
            }
        }
    }
}
