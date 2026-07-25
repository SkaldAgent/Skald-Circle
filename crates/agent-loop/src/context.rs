//! The system context (layered) and the `ContextAssembler` — from system +
//! history to wire messages.
//!
//! **Well-formedness contract** (every assembler MUST honor it):
//!
//! 1. Order: static system → compaction summary (if any) → messages after
//!    `covered_up_to` → dynamic tail → tail reminder.
//! 2. Every assistant `tool_call` has a tool-result: `Done`→result,
//!    `Failed`→error, `Cancelled`/`Rejected`→note, **`Running`/`AwaitingHuman`
//!    surviving a crash → synthetic "interrupted" result**.
//! 3. No `failed` messages (orphans) — already filtered by the store.
//! 4. DTL injection (§4.10 of the blueprint): when `model.tool_rendering` is
//!    not `Inline` and an `ActivationSource` is present, each activation is
//!    projected at its anchor (marker vs system+tools block, append-only).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::activation::{ActivationSource, ToolRendering};
use crate::ids::{ConversationId, FrameId};
use crate::model::ModelInfo;
use crate::store::{HistoryStore, Role, StoredMessage};

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

/// The shipped assembler: system + summary + history, with an optional
/// message window and tool-result truncation. Honors the DTL injection
/// contract when given an `ActivationSource`.
pub struct LinearAssembler {
    /// Keep at most this many history messages (cut at a User/Agent boundary,
    /// never mid assistant+tool group).
    pub max_messages:          Option<usize>,
    /// Truncate each tool result to this many chars.
    pub max_tool_result_chars: Option<usize>,
    /// DTL activations (only consulted when `tool_rendering != Inline`).
    pub activation:            Option<Arc<dyn ActivationSource>>,
}

impl LinearAssembler {
    pub fn new() -> Self {
        Self { max_messages: None, max_tool_result_chars: None, activation: None }
    }

    pub fn with_max_messages(mut self, n: usize) -> Self {
        self.max_messages = Some(n);
        self
    }

    pub fn with_tool_result_limit(mut self, n: usize) -> Self {
        self.max_tool_result_chars = Some(n);
        self
    }

    pub fn with_activation(mut self, src: Arc<dyn ActivationSource>) -> Self {
        self.activation = Some(src);
        self
    }
}

impl Default for LinearAssembler {
    fn default() -> Self { Self::new() }
}

/// The summary block is prefixed so the model understands what it is (Skald
/// keeps its own SUMMARY_PREFIX in its assembler).
pub const SUMMARY_PREFIX: &str = "[CONTEXT SUMMARY — earlier messages were compacted into this summary]";

#[async_trait]
impl ContextAssembler for LinearAssembler {
    async fn build(
        &self,
        store: &Arc<dyn HistoryStore>,
        input: &AssembleInput,
    ) -> crate::Result<Vec<Value>> {
        let mut out: Vec<Value> = Vec::new();

        // 1. static system
        if !input.system.base.is_empty() {
            out.push(json!({ "role": "system", "content": input.system.base }));
        }
        for s in &input.system.extra_static {
            out.push(json!({ "role": "system", "content": s }));
        }

        // 2. summary + surviving history
        let summary = store.latest_summary(input.frame).await?;
        if let Some(s) = &summary {
            out.push(json!({
                "role": "system",
                "content": format!("{SUMMARY_PREFIX}\n\n{}", s.text),
            }));
        }
        let mut history = match &summary {
            Some(s) => store.load_since(input.frame, s.covered_up_to).await?,
            None    => store.load(input.frame).await?,
        };
        if let Some(max) = self.max_messages {
            history = window(history, max);
        }

        // 3. DTL activations (consulted only in non-Inline modes)
        let activations = match (&self.activation, input.model.tool_rendering) {
            (Some(src), ToolRendering::Inline) => {
                let _ = src;
                Vec::new()
            }
            (Some(src), _) => src.activations(input.frame).await.unwrap_or_default(),
            (None, _)      => Vec::new(),
        };

        for msg in &history {
            project_message(&mut out, msg, self.max_tool_result_chars);
            inject_activations(&mut out, msg, &activations, &input.model.tool_rendering);
        }

        // 4. dynamic tail + reminder
        for s in &input.system.dynamic_tail {
            out.push(json!({ "role": "system", "content": s }));
        }
        if let Some(r) = &input.system.tail_reminder {
            out.push(json!({ "role": "system", "content": r }));
        }

        Ok(out)
    }
}

/// Cut the history to at most `max` messages, at a User/Agent boundary so an
/// assistant+tool group is never split.
fn window(history: Vec<StoredMessage>, max: usize) -> Vec<StoredMessage> {
    if history.len() <= max {
        return history;
    }
    let start = history.len() - max;
    let cut = history[start..]
        .iter()
        .position(|m| matches!(m.role, Role::User | Role::Agent))
        .map(|p| start + p)
        .unwrap_or(start);
    history[cut..].to_vec()
}

/// Project one stored message (and its tool results) to wire messages.
fn project_message(out: &mut Vec<Value>, msg: &StoredMessage, result_limit: Option<usize>) {
    match msg.role {
        Role::System => {
            out.push(json!({ "role": "system", "content": msg.content }));
        }
        Role::User | Role::Agent => {
            out.push(json!({ "role": "user", "content": msg.content }));
        }
        Role::Assistant => {
            let mut wire = json!({ "role": "assistant", "content": msg.content });
            if let Some(r) = &msg.reasoning {
                // Echoed under both names: DeepSeek expects reasoning_content,
                // others reasoning (the clients normalize on read).
                wire["reasoning_content"] = json!(r);
            }
            if !msg.calls.is_empty() {
                let calls: Vec<Value> = msg
                    .calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id":   c.provider_id,
                            "type": "function",
                            "function": {
                                "name":      c.name,
                                "arguments": serde_json::to_string(&c.arguments)
                                    .unwrap_or_else(|_| "{}".into()),
                            },
                        })
                    })
                    .collect();
                wire["tool_calls"] = Value::Array(calls);
            }
            out.push(wire);

            for call in &msg.calls {
                let mut content = match call.state {
                    crate::store::CallState::Running | crate::store::CallState::AwaitingHuman => {
                        "[interrupted: this tool call did not complete — the session restarted \
                         before a result was recorded]"
                            .to_string()
                    }
                    _ => call.result.clone().unwrap_or_default(),
                };
                if let Some(limit) = result_limit
                    && content.chars().count() > limit
                {
                    content = format!(
                        "{}… [truncated]",
                        content.chars().take(limit).collect::<String>()
                    );
                }
                out.push(json!({
                    "role":         "tool",
                    "tool_call_id": call.provider_id,
                    "content":      content,
                }));
            }
        }
    }
}

/// DTL injection at an activation anchor (blueprint §4.10):
/// - `DeferredToolReference`: `_tool_references` marker on the FIRST tool
///   result of the anchored assistant message (the client converts it).
/// - `SystemToolBlock`: a `{role:"system", tools:[defs]}` message appended
///   right after the anchored message's tool-result group.
fn inject_activations(
    out: &mut Vec<Value>,
    msg: &StoredMessage,
    activations: &[crate::activation::Activation],
    mode: &ToolRendering,
) {
    let acts: Vec<&crate::activation::Activation> =
        activations.iter().filter(|a| a.anchor == msg.id).collect();
    if acts.is_empty() {
        return;
    }
    match mode {
        ToolRendering::Inline => {}
        ToolRendering::DeferredToolReference => {
            let names: Vec<Value> = acts
                .iter()
                .flat_map(|a| &a.defs)
                .filter_map(|d| d["function"]["name"].as_str())
                .map(|n| json!(n))
                .collect();
            if names.is_empty() {
                return;
            }
            // Attach to the first tool result just emitted for this message.
            if let Some(tool_msg) = out
                .iter_mut()
                .rev()
                .take(msg.calls.len())
                .find(|m| m["role"].as_str() == Some("tool"))
            {
                tool_msg["_tool_references"] = Value::Array(names);
            }
        }
        ToolRendering::SystemToolBlock => {
            let defs: Vec<Value> = acts.iter().flat_map(|a| a.defs.clone()).collect();
            if !defs.is_empty() {
                out.push(json!({ "role": "system", "tools": defs }));
            }
        }
    }
}
