//! The loop event taxonomy and the broadcast bus.
//!
//! Every event is wrapped in [`Event`], tagged with the emitting conversation,
//! frame and parent frame — subscribers (a UI translator, a logger) reconstruct
//! nesting from the tags. Transport: `tokio::sync::broadcast` (multi-subscriber,
//! lag-tolerant).

use serde_json::Value;
use tokio::sync::broadcast;

use crate::ids::{ConversationId, FrameId, MessageId, ModelId, TaskId, ToolCallId};
use crate::model::{ToolCall, Usage};
use crate::store::CallOutcome;

/// Whether a [`LoopEvent::TokenDelta`] carries visible answer text or reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaKind {
    Content,
    Reasoning,
}

/// Events emitted by a running loop. Every variant is wrapped in [`Event`]
/// before hitting the bus, so conversation/frame tags are never optional.
#[derive(Debug, Clone)]
pub enum LoopEvent {
    // ── turn ──
    TurnStarted,
    RoundStarted {
        round: usize,
    },
    UserMessage {
        message_id: MessageId,
        content: String,
        synthetic: bool,
        metadata: Option<Value>,
    },
    TokenDelta {
        kind: DeltaKind,
        text: String,
    },
    Thinking {
        message_id: MessageId,
        content: String,
        usage: Usage,
        reasoning: Option<String>,
    },
    Done {
        message_id: MessageId,
        content: String,
        usage: Usage,
        reasoning: Option<String>,
    },
    // ── tools ──
    ToolCallStarted {
        id: ToolCallId,
        message_id: MessageId,
        name: String,
        args: Value,
    },
    ToolCallFinished {
        id: ToolCallId,
        outcome: CallOutcome,
    },
    ApprovalRequired {
        id: ToolCallId,
        name: String,
        args: Value,
    },
    // ── sub-agents (emitted by child loops; parent_frame in the tag) ──
    AgentSpawned {
        frame: FrameId,
        agent: String,
        depth: u32,
        prompt_preview: String,
    },
    AgentFinished {
        frame: FrameId,
        agent: String,
        result_preview: String,
    },
    AsyncResultReady {
        task: TaskId,
    },
    // ── infrastructure ──
    ModelFallback {
        from: ModelId,
        to: ModelId,
        reason: String,
    },
    LlmFailed {
        tried: Vec<ModelId>,
        last_error: String,
    },
    Compacted {
        frame: FrameId,
        covered_up_to: MessageId,
    },
    Truncated {
        output_tokens: Option<u32>,
    },
    Error(String),
    Cancelled,
    /// Escape hatch for host-specific events (Skald: PendingWrite with diff,
    /// SecurityGroupSelected, …). Other subscribers ignore it.
    Host(Value),
}

/// An event tagged with its emitting scope.
#[derive(Debug, Clone)]
pub struct Event<E> {
    pub conversation: ConversationId,
    pub frame: FrameId,
    pub parent_frame: Option<FrameId>,
    pub inner: E,
}

/// Thin wrapper over the manager's broadcast sender, handed to the kernel,
/// gates, tools and hooks for out-of-band emission. Cheap to clone.
#[derive(Clone)]
pub struct EventSink {
    pub(crate) conversation: ConversationId,
    pub(crate) tx: broadcast::Sender<Event<LoopEvent>>,
}

impl EventSink {
    /// Wrap a bus sender for one conversation. Public so hosts can build
    /// sinks in their own tests and adapters; the kernel builds them via the
    /// manager.
    pub fn new(conversation: ConversationId, tx: broadcast::Sender<Event<LoopEvent>>) -> Self {
        Self { conversation, tx }
    }

    /// Emit an event for a frame. Best-effort: with no subscribers the send
    /// fails silently — events are never load-bearing for the loop's outcome.
    pub fn emit(&self, frame: FrameId, parent_frame: Option<FrameId>, inner: LoopEvent) {
        let _ = self.tx.send(Event {
            conversation: self.conversation.clone(),
            frame,
            parent_frame,
            inner,
        });
    }

    pub fn conversation(&self) -> &ConversationId { &self.conversation }

    /// Recover the sink from a tool's extensions (the kernel inserts one into
    /// every `ToolCtx` it builds, so shipped tools can emit out-of-band).
    pub fn from_extensions(ext: &crate::tool::Extensions) -> Option<EventSink> {
        ext.get::<EventSink>().map(|s| (*s).clone())
    }
}

/// A running tool call, as passed to `LoopHooks::pre_tool_call` (mutable) and
/// `post_tool_call`. Distinct from the model's [`crate::model::ToolCall`]:
/// this one carries the store id allocated before execution.
#[derive(Debug, Clone)]
pub struct PendingToolCall {
    pub id: ToolCallId,
    pub message_id: MessageId,
    pub provider_id: Option<String>,
    pub name: String,
    pub arguments: Value,
}

impl PendingToolCall {
    pub fn wire_call(&self) -> ToolCall {
        ToolCall {
            id: self.provider_id.clone().unwrap_or_default(),
            name: self.name.clone(),
            arguments: self.arguments.clone(),
        }
    }
}
