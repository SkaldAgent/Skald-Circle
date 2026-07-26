//! The projection: stored history → wire messages. **This is where provider
//! divergence lives**, so it belongs to the crate rather than to any host.
//!
//! What the crate owns here: the shape of every message (string content vs
//! content-part array, `cache_control` placement, `tool_calls`/`tool` shapes,
//! media parts), the well-formedness rules (a result for every tool call, no
//! orphans, role alternation, boundary-safe windowing), the dynamic-tool-loading
//! injections, and the byte fidelity of what goes back on the wire.
//!
//! What the host owns: the **content** — the system prompt layers
//! ([`crate::context::SystemContextSource`]), which media a message may inline
//! ([`MediaSource`]) and how an over-long tool result is condensed
//! ([`ToolResultDigest`]). Everything is optional: with no hooks at all the
//! projection is a complete, correct OpenAI-shaped conversation.
//!
//! **Well-formedness contract** (the reason a resumed turn can just re-run):
//!
//! 1. Order: static system → extra static → summary → history after
//!    `covered_up_to` → dynamic tail → tail reminder.
//! 2. Every assistant `tool_call` has a tool result: `Done` → the result,
//!    `Failed` → an error, `Cancelled`/`Rejected` → a note, and a `Running` /
//!    `AwaitingHuman` call that survived a crash → a synthetic "interrupted"
//!    result. A model must never see a call it gets no answer for.
//! 3. No `failed` messages (orphans of cancelled turns) — the store filters them.
//! 4. DTL injections are **append-only**: the cacheable prefix stays
//!    byte-identical, so activating a tool never invalidates the prompt cache.

pub mod media;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::activation::{Activation, ActivationSource, ToolRendering};
use crate::context::AssembleInput;
use crate::ids::MessageId;
use crate::store::{CallState, HistoryStore, Role, StoredCall, StoredMessage};

pub use media::{MediaBlob, MediaBudget, MediaKind};

// ── Configuration ────────────────────────────────────────────────────────────

/// How a stored `reasoning_content` is echoed back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasoningEcho {
    /// `reasoning_content` only (DeepSeek).
    #[default]
    ContentOnly,
    /// Both `reasoning_content` and `reasoning` — some OpenAI-compatible
    /// endpoints read one, some the other, and neither rejects the extra key.
    Both,
}

/// When and how far tool results are shrunk.
#[derive(Debug, Clone, Copy)]
pub struct ResultLimit {
    /// Gate: results longer than this (in bytes — cheap and stable) are shrunk.
    /// The fallback truncation cuts on a **char** boundary, never mid-codepoint.
    pub max_chars: usize,
    /// Shrink only results of turns before the current one, so the in-flight
    /// turn always sees its own tool output in full.
    pub previous_turns_only: bool,
}

/// The protocol-shaped knobs of the projection. [`Default`] is a correct
/// OpenAI-shaped conversation; a host overrides only what its models need.
#[derive(Debug, Clone)]
pub struct Projection {
    /// Header of the compaction summary block.
    pub summary_prefix:         String,
    /// Optional trailer, to mark where the summary ends and full history resumes.
    pub summary_suffix:         Option<String>,
    /// Keep at most this many history messages (cut boundary-safely).
    pub max_messages:           Option<usize>,
    pub max_tool_result:        Option<ResultLimit>,
    /// Result text for a call that was still `Running`/`AwaitingHuman` when the
    /// process died.
    pub interrupted_text:       String,
    /// Result text for a `Rejected` call that recorded none.
    pub rejected_default:       String,
    /// Result text for a `Cancelled` call that recorded none.
    pub cancelled_default:      String,
    /// Some models (DeepSeek thinking mode) reject a replayed tool-calling turn
    /// whose `reasoning_content` is empty: this stands in when none was stored.
    pub reasoning_placeholder:  Option<String>,
    pub reasoning_echo:         ReasoningEcho,
    /// Joins the dynamic-tail layers into the single trailing system message.
    pub tail_separator:         String,
    pub media:                  MediaBudget,
    /// In `DeferredToolReference` mode, the tool whose result carries the
    /// `_tool_references` marker (the activation tool's name). `None` = the
    /// first result of the anchored message.
    pub activation_anchor_tool: Option<String>,
}

/// The default summary header — enough for a model to know what it is reading.
pub const SUMMARY_PREFIX: &str =
    "[CONTEXT SUMMARY — earlier messages were compacted into this summary]";

impl Default for Projection {
    fn default() -> Self {
        Self {
            summary_prefix:         SUMMARY_PREFIX.to_string(),
            summary_suffix:         None,
            max_messages:           None,
            max_tool_result:        None,
            interrupted_text:       "[interrupted: this tool call did not complete — the session \
                                     restarted before a result was recorded]"
                .to_string(),
            rejected_default:       String::new(),
            cancelled_default:      String::new(),
            reasoning_placeholder:  None,
            reasoning_echo:         ReasoningEcho::default(),
            tail_separator:         "\n\n---\n".to_string(),
            media:                  MediaBudget::default(),
            activation_anchor_tool: None,
        }
    }
}

// ── Host hooks ───────────────────────────────────────────────────────────────

/// Which media a message may inline. The host authorizes (containment,
/// ownership, upload rules); the crate decides shape and budget.
#[async_trait]
pub trait MediaSource: Send + Sync {
    /// Media attached to a user/agent message.
    async fn message_media(&self, _msg: &StoredMessage) -> Vec<Arc<dyn MediaBlob>> {
        Vec::new()
    }
    /// Media produced by an assistant turn's tool calls.
    async fn call_media(&self, _calls: &[StoredCall]) -> Vec<Arc<dyn MediaBlob>> {
        Vec::new()
    }
    /// Text appended to the message for the media that did NOT make it (a path
    /// list, so the agent can still reach them with a tool).
    ///
    /// `skipped` are **positions in the vector `message_media` just returned**
    /// for this message, so the host can map them back to whatever it built
    /// them from.
    fn skipped_text(&self, _msg: &StoredMessage, _skipped: &[usize]) -> Option<String> {
        None
    }
}

/// How an over-long tool result is condensed. The crate decides *when*
/// (the [`ResultLimit`] gate); the host decides *what to say*, because a good
/// summary knows what the tool does.
#[async_trait]
pub trait ToolResultDigest: Send + Sync {
    /// `None` → the crate applies its generic char-boundary truncation.
    async fn condense(&self, name: &str, args: &Value, result: &str) -> Option<String>;
}

/// The host hooks, all optional.
#[derive(Default, Clone)]
pub struct ProjectionHooks {
    pub activation: Option<Arc<dyn ActivationSource>>,
    pub media:      Option<Arc<dyn MediaSource>>,
    pub digest:     Option<Arc<dyn ToolResultDigest>>,
}

// ── The engine ───────────────────────────────────────────────────────────────

/// Project a frame's stored history into wire messages.
pub async fn project(
    store: &Arc<dyn HistoryStore>,
    input: &AssembleInput,
    cfg:   &Projection,
    hooks: &ProjectionHooks,
) -> crate::Result<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();

    // 1. Static system message — the cacheable prefix. With prompt caching the
    //    content becomes a one-part array carrying the cache breakpoint.
    if !input.system.base.is_empty() {
        out.push(if input.model.prompt_cache {
            json!({
                "role": "system",
                "content": [{
                    "type": "text",
                    "text": input.system.base,
                    "cache_control": { "type": "ephemeral" },
                }],
            })
        } else {
            json!({ "role": "system", "content": input.system.base })
        });
    }

    // 2. Extra static layers (per-interface rules, session-scoped blocks).
    for s in &input.system.extra_static {
        out.push(json!({ "role": "system", "content": s }));
    }

    // 3. Compaction summary, then the history it did not cover.
    let summary = store.latest_summary(input.frame).await?;
    if let Some(s) = &summary {
        let mut content = format!("{}\n\n{}", cfg.summary_prefix, s.text);
        if let Some(suffix) = &cfg.summary_suffix {
            content.push_str("\n\n");
            content.push_str(suffix);
        }
        out.push(json!({ "role": "system", "content": content }));
    }
    let mut history = match &summary {
        Some(s) => store.load_since(input.frame, s.covered_up_to).await?,
        None    => store.load(input.frame).await?,
    };
    if let Some(max) = cfg.max_messages {
        window(&mut history, max);
    }

    // 4. The conversation.
    let ctx = HistoryCtx::new(&history, cfg, hooks, input).await?;
    for (idx, entry) in history.iter().enumerate() {
        ctx.project_message(&mut out, idx, entry).await;
    }

    // 5. Dynamic tail — the fresh layers, as ONE trailing system message so a
    //    model reads them as a single "current state" block.
    if !input.system.dynamic_tail.is_empty() {
        let tail = input.system.dynamic_tail.join(&cfg.tail_separator);
        if !tail.is_empty() {
            out.push(json!({ "role": "system", "content": tail }));
        }
    }

    // 6. Tail reminder.
    if let Some(r) = &input.system.tail_reminder {
        out.push(json!({ "role": "system", "content": r }));
    }

    Ok(out)
}

/// Cut the history to at most `max` messages. A leading assistant message is
/// dropped as well: a window must not open on half an exchange.
fn window(history: &mut Vec<StoredMessage>, max: usize) {
    if history.len() <= max {
        return;
    }
    history.drain(..history.len() - max);
    if matches!(history.first().map(|m| m.role), Some(Role::Assistant)) {
        history.drain(..1);
    }
}

/// Per-build state shared by every message projection.
struct HistoryCtx<'a> {
    cfg:   &'a Projection,
    hooks: &'a ProjectionHooks,
    model: &'a crate::model::ModelInfo,
    /// Activated tool defs by anchor message (empty in `Inline` mode).
    activations: HashMap<MessageId, Vec<Value>>,
    /// Index of the last `User`/`Agent` message: everything before it belongs
    /// to a previous turn.
    boundary: Option<usize>,
    /// First index of the current turn's group — media is inlined only from
    /// here on, so images are not re-sent (and re-billed) every round.
    media_turn_start: usize,
}

impl<'a> HistoryCtx<'a> {
    async fn new(
        history: &[StoredMessage],
        cfg:     &'a Projection,
        hooks:   &'a ProjectionHooks,
        input:   &'a AssembleInput,
    ) -> crate::Result<Self> {
        let activations = match (&hooks.activation, input.model.tool_rendering) {
            // Inline mode renders activated tools in the `tools` array itself:
            // nothing to inject, so the source is not even consulted.
            (_, ToolRendering::Inline) | (None, _) => HashMap::new(),
            (Some(src), _) => src
                .activations(input.frame)
                .await
                .unwrap_or_default()
                .into_iter()
                .fold(HashMap::<MessageId, Vec<Value>>::new(), |mut acc, a: Activation| {
                    acc.entry(a.anchor).or_default().extend(a.defs);
                    acc
                }),
        };

        let boundary = history
            .iter()
            .rposition(|e| matches!(e.role, Role::User | Role::Agent));

        // Trailing assistant rows are the in-flight turn's own rounds; the
        // current turn's user messages sit just before them.
        let mut media_turn_start = history.len();
        while media_turn_start > 0
            && matches!(history[media_turn_start - 1].role, Role::Assistant)
        {
            media_turn_start -= 1;
        }
        while media_turn_start > 0
            && matches!(history[media_turn_start - 1].role, Role::User | Role::Agent)
        {
            media_turn_start -= 1;
        }

        Ok(Self {
            cfg,
            hooks,
            model: &input.model,
            activations,
            boundary,
            media_turn_start,
        })
    }

    async fn project_message(&self, out: &mut Vec<Value>, idx: usize, entry: &StoredMessage) {
        match entry.role {
            // System messages are BUILT (layers 1-2), never replayed from the
            // store; a host that stores them gets them back verbatim.
            Role::System => out.push(json!({ "role": "system", "content": entry.content })),
            Role::User | Role::Agent => self.push_user(out, idx, entry).await,
            Role::Assistant => self.push_assistant(out, idx, entry).await,
        }
    }

    /// A user/agent message: text plus, for the current turn, inlined media.
    async fn push_user(&self, out: &mut Vec<Value>, idx: usize, entry: &StoredMessage) {
        let mut text = entry.content.clone();
        let mut parts: Vec<Value> = Vec::new();

        if let Some(src) = &self.hooks.media {
            let blobs = src.message_media(entry).await;
            if !blobs.is_empty() {
                // Older turns keep the textual path: everything is "skipped".
                let (inlined, skipped) = if idx >= self.media_turn_start {
                    media::partition(&blobs, &self.model.capabilities, &self.cfg.media).await
                } else {
                    (Vec::new(), (0..blobs.len()).collect())
                };
                if let Some(extra) = src.skipped_text(entry, &skipped) {
                    text.push_str(&extra);
                }
                parts = inlined;
            }
        }

        push_user_chunk(out, text, parts);
    }

    /// An assistant message: the turn itself, then a result for every call, then
    /// the append-only DTL injections.
    async fn push_assistant(&self, out: &mut Vec<Value>, idx: usize, entry: &StoredMessage) {
        let stored_reasoning = entry.reasoning.as_deref().filter(|s| !s.is_empty());

        if entry.calls.is_empty() {
            let mut msg = json!({ "role": "assistant", "content": entry.content });
            if let Some(r) = stored_reasoning {
                self.set_reasoning(&mut msg, r);
            }
            out.push(msg);
            return;
        }

        let calls: Vec<Value> = entry
            .calls
            .iter()
            .map(|c| {
                json!({
                    "id":   c.provider_id,
                    "type": "function",
                    "function": { "name": c.name, "arguments": wire_arguments(c) },
                })
            })
            .collect();
        let mut msg = json!({
            "role":       "assistant",
            "content":    entry.content,
            "tool_calls": calls,
        });
        // A tool-calling turn may need a non-empty reasoning on replay even when
        // none was recorded.
        if let Some(r) = stored_reasoning.or(self.cfg.reasoning_placeholder.as_deref()) {
            self.set_reasoning(&mut msg, r);
        }
        out.push(msg);

        // One result per call, in call order — the model matches them by id.
        let is_previous_turn = self.boundary.is_some_and(|b| idx < b);
        let anchored = self.activations.get(&entry.id);
        let mut marked = false;

        for call in &entry.calls {
            let mut tool_msg = json!({
                "role":         "tool",
                "tool_call_id": call.provider_id,
                "content":      self.result_content(call, is_previous_turn).await,
            });
            // Anthropic DTL: the activation's result carries the marker its
            // client turns into `tool_reference` blocks.
            if self.model.tool_rendering == ToolRendering::DeferredToolReference
                && !marked
                && let Some(defs) = anchored
                && self.is_anchor(call)
            {
                let names: Vec<Value> = defs
                    .iter()
                    .filter_map(|d| d["function"]["name"].as_str())
                    .map(|n| json!(n))
                    .collect();
                if !names.is_empty() {
                    tool_msg["_tool_references"] = Value::Array(names);
                    marked = true;
                }
            }
            out.push(tool_msg);
        }

        // Media a tool produced, as a synthetic user message right after the
        // result group (the current turn only).
        if idx >= self.media_turn_start
            && let Some(src) = &self.hooks.media
        {
            let blobs = src.call_media(&entry.calls).await;
            if !blobs.is_empty() {
                let (parts, _) =
                    media::partition(&blobs, &self.model.capabilities, &self.cfg.media).await;
                if !parts.is_empty() {
                    out.push(json!({ "role": "user", "content": parts }));
                }
            }
        }

        // Kimi-style DTL: the activated defs as a `system` message carrying a
        // `tools` field, appended after the group — the prefix stays identical.
        if self.model.tool_rendering == ToolRendering::SystemToolBlock
            && let Some(defs) = anchored
            && !defs.is_empty()
        {
            out.push(json!({ "role": "system", "tools": defs }));
        }
    }

    fn set_reasoning(&self, msg: &mut Value, reasoning: &str) {
        msg["reasoning_content"] = json!(reasoning);
        if self.cfg.reasoning_echo == ReasoningEcho::Both {
            msg["reasoning"] = json!(reasoning);
        }
    }

    /// Whether this call is the DTL anchor within its message.
    fn is_anchor(&self, call: &StoredCall) -> bool {
        match &self.cfg.activation_anchor_tool {
            Some(name) => &call.name == name,
            None       => true, // the first result of the message
        }
    }

    /// The tool result text: the well-formedness rule of contract point 2, then
    /// the size gate.
    async fn result_content(&self, call: &StoredCall, is_previous_turn: bool) -> String {
        let content = match call.state {
            CallState::Done      => call.result.clone().unwrap_or_default(),
            CallState::Failed    => {
                format!("Error: {}", call.result.as_deref().unwrap_or("unknown error"))
            }
            // A recorded reason wins; an absent or empty one falls back to the
            // configured note — a model must never read an empty tool result
            // and have to guess what happened.
            CallState::Rejected  => non_empty(&call.result)
                .unwrap_or_else(|| self.cfg.rejected_default.clone()),
            CallState::Cancelled => non_empty(&call.result)
                .unwrap_or_else(|| self.cfg.cancelled_default.clone()),
            // Running / AwaitingHuman reaching the projection means the process
            // died mid-flight: the call really was interrupted.
            CallState::Running | CallState::AwaitingHuman => self.cfg.interrupted_text.clone(),
        };

        let Some(limit) = self.cfg.max_tool_result else {
            return content;
        };
        if limit.previous_turns_only && !is_previous_turn {
            return content;
        }
        if content.len() <= limit.max_chars {
            return content;
        }
        if let Some(d) = &self.hooks.digest
            && let Some(short) = d.condense(&call.name, &call.arguments, &content).await
        {
            return short;
        }
        format!(
            "{}… [truncated]",
            content.chars().take(limit.max_chars).collect::<String>()
        )
    }
}

fn non_empty(s: &Option<String>) -> Option<String> {
    s.clone().filter(|s| !s.is_empty())
}

/// The arguments string sent back on the wire. The **raw recorded string** wins:
/// re-serializing a parsed `Value` reorders object keys (serde_json's map is
/// ordered), which would change the bytes the model produced and break the
/// prompt-cache prefix.
fn wire_arguments(call: &StoredCall) -> String {
    match &call.arguments_raw {
        Some(raw) => raw.clone(),
        None => serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".into()),
    }
}

/// Append one user/agent chunk, coalescing with a preceding `user` message —
/// consecutive user rows are one wire message, so strict-alternation APIs stay
/// happy. Media parts keep their position relative to the text.
pub fn push_user_chunk(out: &mut Vec<Value>, text: String, media: Vec<Value>) {
    fn text_part(t: &str) -> Value {
        json!({ "type": "text", "text": t })
    }

    if let Some(last) = out.last_mut()
        && last["role"] == "user"
    {
        if !last["content"].is_array() && media.is_empty() {
            let prev = last["content"].as_str().unwrap_or("").to_string();
            last["content"] = Value::String(format!("{prev}\n\n{text}"));
            return;
        }
        let mut parts = match last["content"].take() {
            Value::Array(a)  => a,
            Value::String(s) => vec![text_part(&s)],
            _                => Vec::new(),
        };
        if let Some(tp) = parts.iter_mut().rev().find(|p| p["type"] == "text") {
            let prev = tp["text"].as_str().unwrap_or("").to_string();
            tp["text"] = Value::String(format!("{prev}\n\n{text}"));
        } else {
            parts.insert(0, text_part(&text));
        }
        parts.extend(media);
        last["content"] = Value::Array(parts);
        return;
    }
    if media.is_empty() {
        out.push(json!({ "role": "user", "content": text }));
    } else {
        let mut parts = vec![text_part(&text)];
        parts.extend(media);
        out.push(json!({ "role": "user", "content": parts }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesces_consecutive_user_messages() {
        let mut out = vec![];
        push_user_chunk(&mut out, "one".into(), vec![]);
        push_user_chunk(&mut out, "two".into(), vec![]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"], "one\n\ntwo");
    }

    #[test]
    fn media_promotes_the_chunk_to_a_parts_array() {
        let mut out = vec![];
        let part = json!({ "type": "image_url", "image_url": { "url": "data:x" } });
        push_user_chunk(&mut out, "look".into(), vec![part.clone()]);
        assert_eq!(out[0]["content"][0]["type"], "text");
        assert_eq!(out[0]["content"][1], part);

        // A following text chunk folds into the LAST text part, keeping the
        // media after it.
        push_user_chunk(&mut out, "more".into(), vec![]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"][0]["text"], "look\n\nmore");
        assert_eq!(out[0]["content"][1], part);
    }

    #[test]
    fn a_non_user_tail_starts_a_new_chunk() {
        let mut out = vec![json!({ "role": "assistant", "content": "hi" })];
        push_user_chunk(&mut out, "next".into(), vec![]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["role"], "user");
    }

    #[test]
    fn raw_arguments_win_over_the_parsed_value() {
        let mut call = StoredCall {
            id:            crate::ids::ToolCallId(1),
            message_id:    MessageId(1),
            provider_id:   "c1".into(),
            name:          "write_file".into(),
            arguments:     json!({ "a": 1, "z": 2 }),
            arguments_raw: Some(r#"{"z":2,"a":1}"#.to_string()),
            state:         CallState::Done,
            result:        None,
            result_kind:   "text".into(),
            extras:        Value::Null,
        };
        assert_eq!(wire_arguments(&call), r#"{"z":2,"a":1}"#);
        call.arguments_raw = None;
        assert_eq!(wire_arguments(&call), r#"{"a":1,"z":2}"#);
    }
}
