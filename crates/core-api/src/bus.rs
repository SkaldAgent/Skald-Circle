//! In-process event bus for chat turns and system events.
//!
//! Every completed chat turn (user message + assistant response) is published
//! here **after** both messages have been persisted to the DB successfully.
//! Context compaction events are published when the compactor completes a
//! summarisation cycle.
//!
//! Subscribers receive a clone of each [`BusEvent`] via a
//! `tokio::sync::broadcast` channel.
//!
//! # Current consumers
//! - `honcho` plugin: processes `UserMessage` and `AssistantResponse` variants.
//!
//! # Usage
//! ```rust,ignore
//! // Producer (ChatSessionHandler):
//! bus.user_message(ChatEvent { ... });
//! bus.assistant_response(ChatEvent { ... });
//!
//! // Producer (ContextCompactor):
//! bus.compaction_done(CompactionEvent { ... });
//!
//! // Consumer (spawn once, keep running):
//! let mut rx = bus.subscribe();
//! tokio::spawn(async move {
//!     loop {
//!         match rx.recv().await {
//!             Ok(BusEvent::UserMessage(e))      => { /* ... */ }
//!             Ok(BusEvent::AssistantResponse(e)) => { /* ... */ }
//!             Ok(BusEvent::CompactionDone(e))    => { /* ... */ }
//!             Err(RecvError::Lagged(n))          => warn!("lagged by {n}"),
//!             Err(RecvError::Closed)             => break,
//!         }
//!     }
//! });
//! ```

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

pub use tokio::sync::broadcast::error::RecvError;

/// Default channel capacity. At 256 events the bus can absorb a burst of 128
/// turns before a slow consumer starts lagging.
const DEFAULT_CAPACITY: usize = 256;

// ── Sub-event types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum ChatEventRole {
    User,
    Assistant,
    /// Sub-agent invocation (role = "agent" in DB).
    Agent,
}

/// Per-tool-call detail attached to an assistant `ChatEvent`.
#[derive(Debug, Clone)]
pub struct ToolCallEvent {
    pub name:      String,
    pub arguments: Option<String>,
    pub result:    Option<String>,
    /// "done" | "failed"
    pub status:    String,
}

/// A single message in a completed chat turn.
#[derive(Debug, Clone)]
pub struct ChatEvent {
    pub session_id:     i64,
    pub stack_id:       i64,
    /// `chat_history.id` for this message.
    pub message_id:     i64,
    pub role:           ChatEventRole,
    pub content:        String,
    /// True for system-generated messages that look like user turns
    /// (TicManager ticks, notification briefings).
    pub is_synthetic:   bool,
    /// True when a real user is actively participating in the session
    /// (web, telegram). False for automated sessions (cron, tic).
    pub is_interactive: bool,
    /// True for short-lived task sessions (cron, tic) that have no
    /// long-term conversational value (e.g. skip Honcho memory sink).
    pub is_ephemeral:   bool,
    /// Non-empty only for assistant messages that triggered tool calls.
    pub tool_calls:     Vec<ToolCallEvent>,
    pub created_at:     DateTime<Utc>,
}

/// Emitted after the compactor successfully persists a new summary.
#[derive(Debug, Clone)]
pub struct CompactionEvent {
    pub session_id:              i64,
    pub stack_id:                i64,
    /// `chat_summaries.id` of the newly created summary row.
    pub summary_id:              i64,
    /// All `chat_history` rows with `id <= covers_up_to_message_id` are now
    /// covered by the summary and will no longer be sent to the LLM raw.
    pub covers_up_to_message_id: i64,
    /// Input token count of the turn that triggered this compaction.
    pub triggered_by_tokens:     u32,
}

// ── Top-level bus event ───────────────────────────────────────────────────────

/// All events that flow through the [`ChatEventBus`].
#[derive(Debug, Clone)]
pub enum BusEvent {
    /// A user (or synthetic) message was saved after a completed turn.
    UserMessage(ChatEvent),
    /// The assistant's final response was saved after a completed turn.
    AssistantResponse(ChatEvent),
    /// The context compactor created a new summary for a stack.
    CompactionDone(CompactionEvent),
}

// ── Bus ───────────────────────────────────────────────────────────────────────

pub struct ChatEventBus {
    tx: broadcast::Sender<BusEvent>,
}

impl ChatEventBus {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish a user message event. No-op if there are no active subscribers.
    pub fn user_message(&self, event: ChatEvent) {
        let _ = self.tx.send(BusEvent::UserMessage(event));
    }

    /// Publish an assistant response event. No-op if there are no active subscribers.
    pub fn assistant_response(&self, event: ChatEvent) {
        let _ = self.tx.send(BusEvent::AssistantResponse(event));
    }

    /// Publish a compaction-done event. No-op if there are no active subscribers.
    pub fn compaction_done(&self, event: CompactionEvent) {
        let _ = self.tx.send(BusEvent::CompactionDone(event));
    }

    /// Returns a new receiver. Each subscriber gets every future event
    /// independently. If the subscriber falls behind by more than the channel
    /// capacity it will receive `RecvError::Lagged(n)` — handle gracefully.
    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.tx.subscribe()
    }
}

impl Default for ChatEventBus {
    fn default() -> Self {
        Self::new()
    }
}
