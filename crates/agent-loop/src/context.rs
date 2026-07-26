//! The system context (layered) and the `ContextAssembler` — from system +
//! history to wire messages.
//!
//! The projection itself lives in [`crate::projection`], which owns the
//! well-formedness contract and every provider-shaped decision. This module is
//! the seam: hosts implement [`SystemContextSource`] to say *what* goes in the
//! system prompt, and [`LinearAssembler`] configures the projection.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::activation::ActivationSource;
use crate::ids::{ConversationId, FrameId};
use crate::model::ModelInfo;
use crate::projection::{
    MediaSource, Projection, ProjectionHooks, ResultLimit, ToolResultDigest,
};
use crate::store::HistoryStore;

// ── SystemContext ────────────────────────────────────────────────────────────

/// The system prompt as LAYERS (the static prefix is cacheable, the dynamic
/// tail is per-turn fresh).
#[derive(Debug, Clone, Default)]
pub struct SystemContext {
    /// The agent's prompt (static, cacheable).
    pub base:          String,
    /// Per-interface extras (e.g. output format rules).
    pub extra_static:  Vec<String>,
    /// Per-turn: date/time, memory, run context.
    pub dynamic_tail:  Vec<String>,
    pub tail_reminder: Option<String>,
}

impl SystemContext {
    pub fn base(s: impl Into<String>) -> Self {
        Self { base: s.into(), ..Default::default() }
    }

    pub fn with_dynamic(mut self, s: impl Into<String>) -> Self {
        self.dynamic_tail.push(s.into());
        self
    }

    pub fn with_static(mut self, s: impl Into<String>) -> Self {
        self.extra_static.push(s.into());
        self
    }

    pub fn with_reminder(mut self, s: impl Into<String>) -> Self {
        self.tail_reminder = Some(s.into());
        self
    }
}

// ── SystemContextSource ──────────────────────────────────────────────────────

/// What the kernel knows about the current turn when asking for the system
/// context.
#[derive(Debug, Clone)]
pub struct TurnInfo {
    pub conversation: ConversationId,
    pub frame:        FrameId,
    pub agent:        String,
    /// The user message that opened the turn (None on resume).
    pub user_message: Option<String>,
}

#[async_trait]
pub trait SystemContextSource: Send + Sync {
    async fn system_context(&self, turn: &TurnInfo) -> crate::Result<SystemContext>;
}

/// A fixed system context (simple hosts, tests).
pub struct StaticSystemContext {
    ctx: SystemContext,
}

impl StaticSystemContext {
    pub fn new(base: impl Into<String>) -> Self {
        Self { ctx: SystemContext::base(base) }
    }
}

#[async_trait]
impl SystemContextSource for StaticSystemContext {
    async fn system_context(&self, _turn: &TurnInfo) -> crate::Result<SystemContext> {
        Ok(self.ctx.clone())
    }
}

// ── ContextAssembler ─────────────────────────────────────────────────────────

pub struct AssembleInput {
    pub frame:  FrameId,
    pub system: SystemContext,
    pub model:  ModelInfo,
    pub round:  usize,
}

#[async_trait]
pub trait ContextAssembler: Send + Sync {
    async fn build(
        &self,
        store: &Arc<dyn HistoryStore>,
        input: &AssembleInput,
    ) -> crate::Result<Vec<Value>>;
}

// ── LinearAssembler ──────────────────────────────────────────────────────────

/// The shipped assembler: a [`Projection`] plus the host hooks it may use.
///
/// Out of the box it produces a correct OpenAI-shaped conversation. A host with
/// stricter models overrides the projection (`with_projection`) and plugs in its
/// media authorization and result-digest policy.
pub struct LinearAssembler {
    pub projection: Projection,
    pub hooks:      ProjectionHooks,
}

impl LinearAssembler {
    pub fn new() -> Self {
        Self { projection: Projection::default(), hooks: ProjectionHooks::default() }
    }

    /// Replace the whole protocol configuration.
    pub fn with_projection(mut self, projection: Projection) -> Self {
        self.projection = projection;
        self
    }

    /// Keep at most this many history messages (cut boundary-safely).
    pub fn with_max_messages(mut self, n: usize) -> Self {
        self.projection.max_messages = Some(n);
        self
    }

    /// Shrink every tool result longer than `n` chars.
    pub fn with_tool_result_limit(mut self, n: usize) -> Self {
        self.projection.max_tool_result =
            Some(ResultLimit { max_chars: n, previous_turns_only: false });
        self
    }

    /// DTL activations (consulted only when `tool_rendering != Inline`).
    pub fn with_activation(mut self, src: Arc<dyn ActivationSource>) -> Self {
        self.hooks.activation = Some(src);
        self
    }

    /// Which media a message may inline.
    pub fn with_media(mut self, src: Arc<dyn MediaSource>) -> Self {
        self.hooks.media = Some(src);
        self
    }

    /// How an over-long tool result is condensed.
    pub fn with_digest(mut self, digest: Arc<dyn ToolResultDigest>) -> Self {
        self.hooks.digest = Some(digest);
        self
    }
}

impl Default for LinearAssembler {
    fn default() -> Self { Self::new() }
}

/// Re-exported for hosts that only need the default summary header.
pub use crate::projection::SUMMARY_PREFIX;

#[async_trait]
impl ContextAssembler for LinearAssembler {
    async fn build(
        &self,
        store: &Arc<dyn HistoryStore>,
        input: &AssembleInput,
    ) -> crate::Result<Vec<Value>> {
        crate::projection::project(store, input, &self.projection, &self.hooks).await
    }
}
