//! Shared recording of a single tool-call outcome.
//!
//! The persist-then-emit tail of a tool call (`ExecutionOutcome` → DB row +
//! `ToolDone`/`ToolError`/`ToolCancelled` event) was copy-pasted into both the live
//! loop (`run_agent_turn`) and `resume_pending_tools`. `record_tool_outcome` is the
//! single implementation both call.

use serde_json::Value;
use tracing::{debug, info, warn};

use crate::chat_event_bus::ToolCallEvent;
use crate::db::chat_llm_tools;
use crate::tools::{is_file_write_tool, ExecutionOutcome};

use super::ChatSessionHandler;
use super::dispatch::WritePreview;
use super::emitter::TurnEmitter;

/// Whether the enclosing loop should keep going after an outcome is recorded.
pub(super) enum RecordFlow {
    /// Continue with the next tool call / round.
    Continue,
    /// The tool was cancelled by the user — the caller must end the turn.
    Abort,
}

impl ChatSessionHandler {
    /// Persists one tool-call outcome and emits the matching lifecycle event.
    /// Returns [`RecordFlow::Abort`] for a user cancellation (the caller ends the
    /// turn), [`RecordFlow::Continue`] otherwise.
    ///
    /// When `accumulate` is `Some` (the live turn), the call is also appended to the
    /// turn's `ToolCallEvent` list for the chat-event bus, and a `FileChanged` event
    /// is emitted for a successful file-write tool. `resume_pending_tools` passes
    /// `None`: it neither accumulates nor re-emits `FileChanged`.
    pub(super) async fn record_tool_outcome(
        &self,
        tool_call_id: i64,
        tool_name:    &str,
        args:         &Value,
        outcome:      ExecutionOutcome,
        preview:      Option<WritePreview>,
        em:           &TurnEmitter<'_>,
        accumulate:   Option<&mut Vec<ToolCallEvent>>,
    ) -> anyhow::Result<RecordFlow> {
        let pool = &self.db;
        match outcome {
            ExecutionOutcome::Completed(result) => {
                let wire = result.to_wire();
                let kind = result.kind();
                debug!(session_id = self.session_id, tool = %tool_name, tool_call_id, result_len = wire.len(), "tool done");
                chat_llm_tools::complete(pool, tool_call_id, &wire, kind).await?;
                // Media the tool produced (e.g. read_file on an image/PDF) rides
                // out of band in the `media` column; the message builder inlines it
                // as a synthetic user message for a capable model on the current turn.
                let media = result.media();
                if !media.is_empty() {
                    let media_json = serde_json::to_string(media).unwrap_or_else(|_| "[]".to_string());
                    chat_llm_tools::set_media(pool, tool_call_id, &media_json).await?;
                }
                // Persist a file-write's diff snapshot so it re-renders after a reload,
                // and carry it on the event so an auto-allowed write shows the diff live.
                let (preview_old, preview_new) = match preview {
                    Some(WritePreview { old, new }) => {
                        chat_llm_tools::set_preview(pool, tool_call_id, old.as_deref(), new.as_deref()).await?;
                        (old, new)
                    }
                    None => (None, None),
                };
                if let Some(acc) = accumulate {
                    if is_file_write_tool(tool_name)
                        && let Some(p) = args["path"].as_str()
                    {
                        em.file_changed(crate::approval::normalize_path(p)).await;
                    }
                    acc.push(ToolCallEvent {
                        name:      tool_name.to_string(),
                        arguments: Some(serde_json::to_string(args).unwrap_or_default()),
                        result:    Some(wire.clone()),
                        status:    "done".to_string(),
                    });
                }
                em.tool_done(tool_call_id, wire, kind.to_string(), preview_old, preview_new).await;
                Ok(RecordFlow::Continue)
            }
            ExecutionOutcome::Failed(msg) => {
                warn!(session_id = self.session_id, tool = %tool_name, tool_call_id, error = %msg, "tool failed");
                chat_llm_tools::fail(pool, tool_call_id, &msg).await?;
                if let Some(acc) = accumulate {
                    acc.push(ToolCallEvent {
                        name:      tool_name.to_string(),
                        arguments: Some(serde_json::to_string(args).unwrap_or_default()),
                        result:    Some(msg.clone()),
                        status:    "failed".to_string(),
                    });
                }
                em.tool_error(tool_call_id, msg).await;
                Ok(RecordFlow::Continue)
            }
            ExecutionOutcome::Cancelled => {
                // A /stop hit this tool mid-flight. Record it as cancelled (not
                // failed); the sticky token cancels the rest of the loop by
                // construction, so the caller just ends the turn.
                info!(session_id = self.session_id, tool = %tool_name, tool_call_id, "tool cancelled by user");
                chat_llm_tools::cancel(pool, tool_call_id, "Cancelled by user.").await?;
                em.tool_cancelled(tool_call_id).await;
                Ok(RecordFlow::Abort)
            }
        }
    }
}
