//! `Gate` — the pre-execution decision point (policy and/or human). It MAY
//! block waiting for a human: the implementation decides (oneshot, UI, …).
//! Before suspending, an implementation marks the call `AwaitingHuman` via the
//! store (durability) and emits `LoopEvent::ApprovalRequired`.

use async_trait::async_trait;
use serde_json::Value;

use crate::events::EventSink;
use crate::ids::{FrameId, ToolCallId};
use crate::tool::Extensions;

/// A tool call awaiting a gate decision.
#[derive(Debug, Clone)]
pub struct PendingCall {
    pub id:         ToolCallId,
    pub name:       String,
    pub args:       Value,
    pub frame:      FrameId,
    pub agent:      String,
    /// Host free-form (source, permission group, …).
    pub extensions: Extensions,
}

/// The gate's verdict.
#[derive(Debug, Clone)]
pub enum GateDecision {
    Allow,
    Reject { reason: String },
    /// The gate was waiting for a human and the channel closed: the turn ends
    /// and the call STAYS `AwaitingHuman` (the gate marked it before
    /// suspending) — the same semantics as `ToolFailure::Suspend`.
    Suspend,
}

#[async_trait]
pub trait Gate: Send + Sync {
    /// Decide on a call. MAY block awaiting a human — in that case the
    /// implementation marks the call `AwaitingHuman` first (via the store the
    /// host gave it) and emits `ApprovalRequired` on `events`.
    async fn check(&self, call: &PendingCall, events: &EventSink) -> GateDecision;
}

/// Everything runs. The default for simple hosts and tests.
pub struct AllowAll;

#[async_trait]
impl Gate for AllowAll {
    async fn check(&self, _call: &PendingCall, _events: &EventSink) -> GateDecision {
        GateDecision::Allow
    }
}

/// Reject calls whose name matches a pattern: exact, or `prefix*`.
pub struct DenyList {
    patterns: Vec<String>,
}

impl DenyList {
    pub fn new(patterns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { patterns: patterns.into_iter().map(Into::into).collect() }
    }

    fn matches(&self, name: &str) -> bool {
        self.patterns.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None         => name == p,
        })
    }
}

#[async_trait]
impl Gate for DenyList {
    async fn check(&self, call: &PendingCall, _events: &EventSink) -> GateDecision {
        if self.matches(&call.name) {
            GateDecision::Reject { reason: format!("tool '{}' denied by policy", call.name) }
        } else {
            GateDecision::Allow
        }
    }
}
