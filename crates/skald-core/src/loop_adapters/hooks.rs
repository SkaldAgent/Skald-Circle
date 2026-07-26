//! Skald's `LoopHooks`: the file-write diff preview bracket (pre: capture the
//! old content; post: capture the new one and persist via `set_call_extras`).
//! Port of the `execute_tool_call` preview bracketing (blueprint §10).

use std::collections::HashMap;
use std::sync::Mutex;

use agent_loop::events::PendingToolCall;
use agent_loop::hooks::{HookCtx, LoopHooks};
use agent_loop::store::CallOutcome;
use serde_json::json;

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
