//! `InMemoryStore` — the shipped non-persistent store (chat not persisted;
//! testing; simple hosts). Monotonic ids per the store contract.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::ids::{ConversationId, FrameId, MessageId, SummaryId, ToolCallId};
use crate::model::Usage;
use crate::store::{
    CallOutcome, CallState, FrameRecord, FrameSpec, HistoryStore, NewCall, NewMessage, NewSummary,
    StoredCall, StoredMessage, StoredSummary,
};

#[derive(Default)]
struct Inner {
    frames:    HashMap<FrameId, FrameRecord>,
    messages:  HashMap<FrameId, Vec<StoredMessage>>,
    calls:     HashMap<MessageId, Vec<StoredCall>>,
    summaries: HashMap<FrameId, Vec<StoredSummary>>,
    next_frame:   i64,
    next_msg:     i64,
    next_call:    i64,
    next_summary: i64,
}

/// Non-persistent store. A "crash" loses everything — which is exactly why
/// it's also the natural target for recovery scenario tests (build the
/// post-crash state by hand).
pub struct InMemoryStore {
    inner: Mutex<Inner>,
}

impl InMemoryStore {
    pub fn new() -> Self { Self { inner: Mutex::new(Inner::default()) } }
}

impl Default for InMemoryStore {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl HistoryStore for InMemoryStore {
    async fn open_frame(
        &self,
        conv:   &ConversationId,
        parent: Option<FrameId>,
        spec:   FrameSpec,
    ) -> crate::Result<FrameId> {
        let mut i = self.inner.lock().unwrap();
        i.next_frame += 1;
        let id = FrameId(i.next_frame);
        i.frames.insert(id, FrameRecord {
            id,
            conversation: conv.clone(),
            parent,
            spec,
            active: true,
        });
        Ok(id)
    }

    async fn close_frame(&self, frame: FrameId) -> crate::Result<()> {
        let mut i = self.inner.lock().unwrap();
        if let Some(f) = i.frames.get_mut(&frame) {
            f.active = false;
        }
        Ok(())
    }

    async fn active_frames(&self, conv: &ConversationId) -> crate::Result<Vec<FrameRecord>> {
        let i = self.inner.lock().unwrap();
        Ok(i.frames.values().filter(|f| f.active && &f.conversation == conv).cloned().collect())
    }

    async fn deepest_active(&self, conv: &ConversationId) -> crate::Result<Option<FrameRecord>> {
        let i = self.inner.lock().unwrap();
        Ok(i.frames
            .values()
            .filter(|f| f.active && &f.conversation == conv)
            .max_by_key(|f| f.spec.depth)
            .cloned())
    }

    async fn append(&self, frame: FrameId, msg: NewMessage) -> crate::Result<MessageId> {
        let mut i = self.inner.lock().unwrap();
        i.next_msg += 1;
        let id = MessageId(i.next_msg);
        i.messages.entry(frame).or_default().push(StoredMessage {
            id,
            role: msg.role,
            content: msg.content,
            reasoning: msg.reasoning,
            synthetic: msg.synthetic,
            failed: false,
            metadata: msg.metadata,
            usage: Usage::default(),
            calls: Vec::new(),
        });
        Ok(id)
    }

    async fn set_usage(&self, msg: MessageId, usage: &Usage) -> crate::Result<()> {
        let mut i = self.inner.lock().unwrap();
        for msgs in i.messages.values_mut() {
            if let Some(m) = msgs.iter_mut().find(|m| m.id == msg) {
                m.usage = usage.clone();
                return Ok(());
            }
        }
        Ok(())
    }

    async fn load(&self, frame: FrameId) -> crate::Result<Vec<StoredMessage>> {
        let i = self.inner.lock().unwrap();
        Ok(load_frame(&i, frame, None))
    }

    async fn load_since(&self, frame: FrameId, after: MessageId) -> crate::Result<Vec<StoredMessage>> {
        let i = self.inner.lock().unwrap();
        Ok(load_frame(&i, frame, Some(after)))
    }

    async fn last(&self, frame: FrameId) -> crate::Result<Option<StoredMessage>> {
        let i = self.inner.lock().unwrap();
        Ok(load_frame(&i, frame, None).into_iter().last())
    }

    async fn mark_failed(&self, msg: MessageId) -> crate::Result<()> {
        let mut i = self.inner.lock().unwrap();
        for msgs in i.messages.values_mut() {
            if let Some(m) = msgs.iter_mut().find(|m| m.id == msg) {
                m.failed = true;
                return Ok(());
            }
        }
        Ok(())
    }

    async fn append_call(&self, msg: MessageId, call: NewCall) -> crate::Result<ToolCallId> {
        let mut i = self.inner.lock().unwrap();
        i.next_call += 1;
        let id = ToolCallId(i.next_call);
        let provider_id = call.provider_id.unwrap_or_else(|| format!("call_{}", id.get()));
        let stored = StoredCall {
            id,
            message_id: msg,
            provider_id,
            name: call.name,
            arguments: call.arguments,
            state: CallState::Running,
            result: None,
            result_kind: String::new(),
            extras: serde_json::Value::Null,
        };
        i.calls.entry(msg).or_default().push(stored.clone());
        // Keep the nested copy inside the message in sync.
        for msgs in i.messages.values_mut() {
            if let Some(m) = msgs.iter_mut().find(|m| m.id == msg) {
                m.calls.push(stored);
                break;
            }
        }
        Ok(id)
    }

    async fn resolve_call(&self, id: ToolCallId, outcome: &CallOutcome) -> crate::Result<()> {
        let mut i = self.inner.lock().unwrap();
        update_call(&mut i, id, |c| {
            c.state = outcome.state();
            c.result = Some(outcome.result_text());
            c.result_kind = outcome.result_kind().to_string();
        });
        Ok(())
    }

    async fn set_call_state(&self, id: ToolCallId, state: CallState) -> crate::Result<()> {
        anyhow::ensure!(
            !state.is_terminal(),
            "set_call_state is only for Running → AwaitingHuman, not terminal {state:?}"
        );
        let mut i = self.inner.lock().unwrap();
        update_call(&mut i, id, |c| c.state = state);
        Ok(())
    }

    async fn calls_in_state(&self, frame: FrameId, states: &[CallState]) -> crate::Result<Vec<StoredCall>> {
        let i = self.inner.lock().unwrap();
        Ok(i.messages
            .get(&frame)
            .map(|msgs| {
                msgs.iter()
                    .flat_map(|m| &m.calls)
                    .filter(|c| states.contains(&c.state))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn save_summary(&self, frame: FrameId, s: NewSummary) -> crate::Result<SummaryId> {
        let mut i = self.inner.lock().unwrap();
        i.next_summary += 1;
        let id = SummaryId(i.next_summary);
        i.summaries.entry(frame).or_default().push(StoredSummary {
            id,
            text: s.text,
            covered_up_to: s.covered_up_to,
        });
        Ok(id)
    }

    async fn latest_summary(&self, frame: FrameId) -> crate::Result<Option<StoredSummary>> {
        let i = self.inner.lock().unwrap();
        Ok(i.summaries.get(&frame).and_then(|v| v.last()).cloned())
    }
}

/// Load a frame's history with calls nested, excluding failed messages,
/// optionally only messages after `after`.
fn load_frame(i: &Inner, frame: FrameId, after: Option<MessageId>) -> Vec<StoredMessage> {
    i.messages
        .get(&frame)
        .map(|msgs| {
            msgs.iter()
                .filter(|m| !m.failed)
                .filter(|m| after.is_none_or(|a| m.id > a))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Apply a mutation to a call both in the by-message index and in the nested
/// copy inside its message.
fn update_call(i: &mut Inner, id: ToolCallId, f: impl Fn(&mut StoredCall)) {
    let mut msg_id = None;
    for calls in i.calls.values_mut() {
        if let Some(c) = calls.iter_mut().find(|c| c.id == id) {
            f(c);
            msg_id = Some(c.message_id);
            break;
        }
    }
    if let Some(msg_id) = msg_id {
        for msgs in i.messages.values_mut() {
            if let Some(m) = msgs.iter_mut().find(|m| m.id == msg_id) {
                if let Some(c) = m.calls.iter_mut().find(|c| c.id == id) {
                    f(c);
                }
                break;
            }
        }
    }
}
