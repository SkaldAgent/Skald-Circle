//! Skald's `LoopHooks` — the two app-specific things that happen around the
//! loop, neither of which the kernel should know about:
//!
//! - [`SkaldWritePreviewHook`]: the file-write diff bracket (pre: capture the
//!   old content; post: the new one, persisted via `set_call_extras`).
//! - [`DtlReanchorHook`]: after a compaction, move dynamic-tool activations off
//!   the messages that just went away.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_loop::events::PendingToolCall;
use agent_loop::hooks::{HookCtx, LoopHooks};
use agent_loop::ids::{FrameId, MessageId};
use agent_loop::store::CallOutcome;
use serde_json::json;
use sqlx::SqlitePool;
use tracing::warn;

use crate::loop_adapters::preview::{PreviewContext, cap_preview, read_current_content};
use crate::tools::is_file_write_tool;

/// Captures before/after snapshots around file-write tools so the diff
/// renders inline and survives a reload.
pub struct SkaldWritePreviewHook {
    ctx: PreviewContext,
    /// old-content captured in `pre_tool_call`, consumed in `post_tool_call`.
    pending: Mutex<HashMap<i64, Option<String>>>,
}

impl SkaldWritePreviewHook {
    pub fn new(ctx: PreviewContext) -> Self {
        Self { ctx, pending: Mutex::new(HashMap::new()) }
    }
}

#[agent_loop::async_trait]
impl LoopHooks for SkaldWritePreviewHook {
    async fn pre_tool_call(&self, call: &mut PendingToolCall, _ctx: &HookCtx) -> agent_loop::hooks::HookVerdict {
        if is_file_write_tool(&call.name)
            && let Some(path) = call.arguments["path"].as_str()
        {
            let old = cap_preview(read_current_content(&self.ctx, path).await);
            self.pending.lock().unwrap().insert(call.id.get(), old);
        }
        agent_loop::hooks::HookVerdict::Allow
    }

    async fn post_tool_call(&self, call: &PendingToolCall, outcome: &CallOutcome, ctx: &HookCtx) {
        let Some(old) = self.pending.lock().unwrap().remove(&call.id.get()) else {
            return;
        };
        let Some(path) = call.arguments["path"].as_str() else {
            return;
        };
        // `new` is captured only on success — a failed/cancelled write shows
        // no diff (the file may not exist in its intended form).
        let new = if matches!(outcome, CallOutcome::Completed(_)) {
            cap_preview(read_current_content(&self.ctx, path).await)
        } else {
            None
        };
        let _ = ctx
            .store
            .set_call_extras(call.id, json!({ "preview_old": old, "preview_new": new }))
            .await;
    }
}

// ── DtlReanchorHook ──────────────────────────────────────────────────────────

/// Keeps dynamic tool loading working across a compaction.
///
/// An activation is pinned to the message whose round activated it — that is
/// where its `tool_reference` marker or its `system`+`tools` block renders. When
/// compaction summarises that message away, the activation would render nowhere
/// and the model would silently lose tools it had already loaded. Re-anchoring
/// them onto the first surviving message keeps them exactly where the
/// projection can still find them.
///
/// Best-effort: a failure costs the model one re-activation, never a wrong
/// answer, so it is logged rather than propagated.
pub struct DtlReanchorHook {
    pool: Arc<SqlitePool>,
}

impl DtlReanchorHook {
    pub fn new(pool: Arc<SqlitePool>) -> Self {
        Self { pool }
    }
}

#[agent_loop::async_trait]
impl LoopHooks for DtlReanchorHook {
    async fn on_compacted(&self, frame: FrameId, covered: MessageId, first_surviving: MessageId) {
        if let Err(e) = crate::db::activated_tools::reanchor_compacted(
            &self.pool,
            frame.get(),
            covered.get(),
            first_surviving.get(),
        )
        .await
        {
            warn!(frame = %frame, error = %e, "failed to re-anchor DTL activations after compaction");
        }
    }
}
