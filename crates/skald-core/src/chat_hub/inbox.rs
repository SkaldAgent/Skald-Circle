//! Per-source input inbox for ChatHub.
//!
//! Each interactive source (telegram, web, mobile…) gets one `SourceInbox` and a
//! single consumer task (spawned lazily in `ChatHub`). A single consumer per
//! source makes delivery strictly FIFO, removing the ordering race of the old
//! detached-spawn dispatch.
//!
//! Messages are kept as **individual** units — they are not coalesced here. The
//! consumer pops one to seed a turn (`build_unit`); any further messages that
//! pile up while the turn runs are drained, one row each, at the turn's round
//! boundaries (`drain_leading_user`) and injected live into the running turn.
//! Coalescing for the LLM (merging consecutive user rows into one `role:user`)
//! happens later in the projection, not here, so the DB keeps each message
//! distinct while the model still sees a single clean user turn.
//!
//! Serialization of the turns themselves still lives in
//! `ChatSessionHandler.processing`; this inbox sits in front of it, adding ordering.

use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;

use tokio::sync::{Mutex, Notify};

use core_api::chat_hub::SendMessageOptions;
use core_api::message_meta::MessageMetadata;

/// One queued user message awaiting dispatch.
pub(super) struct QueuedMessage {
    pub prompt: String,
    pub opts:   SendMessageOptions,
}

/// Pending queue + wake signal for a single source.
#[derive(Default)]
pub(super) struct SourceInbox {
    pub pending: Mutex<VecDeque<QueuedMessage>>,
    pub notify:  Notify,
    /// Bumped by `ChatHub::cancel` (after clearing `pending`) so the consumer can
    /// drop a unit it drained microseconds before a `/stop`.
    pub cancel_epoch: AtomicU64,
}

/// Pops the next dispatch unit from `pending` — a **single** message, used by the
/// consumer to seed a turn. No coalescing: any further queued messages are drained
/// into the running turn at its round boundaries (see `drain_leading_user`).
///
/// Empty queue → `None`. Synthetic messages (notification/event triage) and plain user
/// messages are treated identically here; only `drain_leading_user` distinguishes
/// them, leaving synthetic ones for the notification path.
pub(super) fn build_unit(
    pending: &mut VecDeque<QueuedMessage>,
) -> Option<(String, SendMessageOptions)> {
    let m = pending.pop_front()?;
    Some((m.prompt, m.opts))
}

/// One drained user message ready to be appended to history mid-turn.
pub(super) struct DrainedMessage {
    pub content:  String,
    pub metadata: Option<MessageMetadata>,
}

/// Drains the leading run of **non-synthetic** messages, returning them
/// individually (no coalescing). Stops at the first synthetic message, which is
/// left in the queue for the notification path. Used by the running turn to
/// inject newly-queued user input at a round boundary.
pub(super) fn drain_leading_user(
    pending: &mut VecDeque<QueuedMessage>,
) -> Vec<DrainedMessage> {
    let mut out = Vec::new();
    while pending.front().is_some_and(|m| !m.opts.is_synthetic) {
        let mut m = pending.pop_front().unwrap();
        out.push(DrainedMessage {
            content:  m.prompt,
            metadata: m.opts.metadata.take(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(prompt: &str, synthetic: bool) -> QueuedMessage {
        QueuedMessage {
            prompt: prompt.to_string(),
            opts: SendMessageOptions { is_synthetic: synthetic, ..Default::default() },
        }
    }

    #[test]
    fn empty_queue_yields_none() {
        let mut q = VecDeque::new();
        assert!(build_unit(&mut q).is_none());
    }

    #[test]
    fn build_unit_pops_a_single_message() {
        let mut q = VecDeque::from(vec![msg("hello", false), msg("also this", false)]);
        let (prompt, _) = build_unit(&mut q).unwrap();
        assert_eq!(prompt, "hello");
        // The second message is left for the round-boundary drain.
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn drain_returns_leading_user_messages_individually() {
        let mut q = VecDeque::from(vec![msg("a", false), msg("b", false)]);
        let drained = drain_leading_user(&mut q);
        let contents: Vec<_> = drained.iter().map(|d| d.content.as_str()).collect();
        assert_eq!(contents, vec!["a", "b"]);
        assert!(q.is_empty());
    }

    #[test]
    fn drain_stops_at_a_synthetic_boundary() {
        let mut q = VecDeque::from(vec![
            msg("a", false),
            msg("b", false),
            msg("notification", true),
        ]);
        let drained = drain_leading_user(&mut q);
        let contents: Vec<_> = drained.iter().map(|d| d.content.as_str()).collect();
        assert_eq!(contents, vec!["a", "b"]);
        assert_eq!(q.len(), 1); // the synthetic message is left for the next unit
    }

    #[test]
    fn drain_skips_leading_synthetic() {
        let mut q = VecDeque::from(vec![msg("notification", true), msg("user text", false)]);
        let drained = drain_leading_user(&mut q);
        assert!(drained.is_empty());
        assert_eq!(q.len(), 2);
    }

    fn msg_with_attachment(prompt: &str, path: &str) -> QueuedMessage {
        use core_api::message_meta::{Attachment, MessageMetadata};
        QueuedMessage {
            prompt: prompt.to_string(),
            opts: SendMessageOptions {
                metadata: Some(MessageMetadata {
                    attachments: vec![Attachment {
                        path: path.to_string(),
                        name: path.to_string(),
                        mimetype: None,
                        filesize: None,
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
        }
    }

    #[test]
    fn drain_preserves_per_message_attachments() {
        let mut q = VecDeque::from(vec![
            msg_with_attachment("first", "a.pdf"),
            msg_with_attachment("second", "b.pdf"),
        ]);
        let drained = drain_leading_user(&mut q);
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].metadata.as_ref().unwrap().attachments[0].path, "a.pdf");
        assert_eq!(drained[1].metadata.as_ref().unwrap().attachments[0].path, "b.pdf");
    }
}
