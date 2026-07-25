//! `LoopHooks` — the passive/active interception seam. Every host special-case
//! (diff-preview bracketing, per-tool arg normalization, telemetry, discovery)
//! lives here, not in the kernel. All methods default to no-op.

use std::sync::Arc;

use async_trait::async_trait;

use crate::events::{EventSink, PendingToolCall};
use crate::ids::{ConversationId, FrameId, MessageId};
use crate::kernel::TurnOutcome;
use crate::store::{CallOutcome, HistoryStore};

/// Verdict of `pre_tool_call`.
#[derive(Debug, Clone)]
pub enum HookVerdict {
    Allow,
    Reject { reason: String },
}

/// Context handed to every hook.
pub struct HookCtx {
    pub conversation: ConversationId,
    pub frame:        FrameId,
    pub agent:        String,
    pub store:        Arc<dyn HistoryStore>,
    pub events:       EventSink,
}

#[async_trait]
pub trait LoopHooks: Send + Sync {
    async fn before_round(&self, _round: usize, _ctx: &HookCtx) {}
    async fn after_round(&self, _round: usize, _ctx: &HookCtx) {}

    /// May MUTATE the call's arguments or veto it (Reject). Covers diff-preview
    /// bracketing and per-tool normalizations.
    async fn pre_tool_call(&self, _call: &mut PendingToolCall, _ctx: &HookCtx) -> HookVerdict {
        HookVerdict::Allow
    }

    /// Covers persistence of activated tools, discovery, file-change
    /// notifications, telemetry.
    async fn post_tool_call(&self, _call: &PendingToolCall, _outcome: &CallOutcome, _ctx: &HookCtx) {}

    async fn on_turn_end(&self, _outcome: &TurnOutcome, _ctx: &HookCtx) {}

    /// Fired after a compaction (blueprint §9): hosts re-anchor DTL
    /// activations to the first surviving message here.
    async fn on_compacted(&self, _frame: FrameId, _covered: MessageId, _first_surviving: MessageId) {}
}
