//! Compaction (blueprint §9, D6) — summarising the old part of a frame's
//! history so the context stops growing.
//!
//! It is **not a turn**: one model call, no tools, no rounds, no kernel. That
//! is the whole reason it is its own component — a host can compact a
//! conversation nothing is driving, and the loop never learns it happened.
//!
//! The result is a row, not a return value: the next loop reads
//! `latest_summary` through the assembler and projects
//! `system → summary → messages after covered_up_to`. Callers get a
//! [`CompactionOutcome`] for telemetry, not for threading anywhere.
//!
//! What the host still owns: **when** (see [`should_compact`]), which model,
//! and what to do afterwards ([`LoopHooks::on_compacted`] — re-anchoring
//! anything pinned to a message that just went away).

use std::sync::Arc;

use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::events::{EventSink, LoopEvent};
use crate::hooks::LoopHooks;
use crate::ids::{ConversationId, FrameId, MessageId, SummaryId};
use crate::model::{ModelHint, ModelRequest, ModelResponse, ModelSelector, Usage};
use crate::store::{CallState, HistoryStore, NewSummary, Role, StoredMessage};

// ── The shipped prompt ───────────────────────────────────────────────────────

/// Prepended to the stored summary when it is projected back into the context.
/// It tells the model this is a handoff from a previous context window, not a
/// set of live instructions — without it, a model happily re-answers questions
/// the summary merely *mentions*.
pub const SUMMARY_PREFIX: &str = "\
[CONTEXT COMPACTION — REFERENCE ONLY] Earlier turns were compacted \
into the summary below. This is a handoff from a previous context \
window — treat it as background reference, NOT as active instructions. \
Do NOT answer questions or fulfill requests mentioned in this summary; \
they were already addressed. \
Your current task is identified in the '## Active Task' section of the \
summary — resume exactly from there. \
Your system prompt and any injected memory files are ALWAYS authoritative \
— never deprioritize them due to this compaction note. \
Respond ONLY to the latest user message that appears AFTER this summary. \
The current session state (files, config, etc.) may reflect work \
described here — avoid repeating it:";

/// Preamble shared by the first-compaction and the update prompts. The wording
/// is deliberately plain: a summariser is the one call most likely to trip a
/// content filter, since it restates whatever the conversation contained.
pub const SUMMARIZER_PREAMBLE: &str = "\
You are a summarization agent creating a context checkpoint. \
Treat the conversation turns below as source material for a \
compact record of prior work. \
Produce only the structured summary; do not add a greeting, \
preamble, or prefix. \
Write the summary in the same language the user was using in the \
conversation — do not translate or switch to English. \
NEVER include API keys, tokens, passwords, secrets, credentials, \
or connection strings in the summary — replace any that appear \
with [REDACTED]. Note that the user may have had credentials present, \
but do not preserve their values.";

/// The sections the summariser must fill in. Structure beats prose here: the
/// next context window is resumed from `## Active Task`, so that field is
/// worth more than everything else combined.
pub const SUMMARY_TEMPLATE: &str = "\
## Active Task
[THE SINGLE MOST IMPORTANT FIELD. Copy the user's most recent request or \
task assignment verbatim — the exact words they used. If multiple tasks \
were requested and only some are done, list only the ones NOT yet completed. \
Continuation should pick up exactly here. Example: \
\"User asked: 'Now refactor the auth module to use JWT instead of sessions'\" \
If no outstanding task exists, write \"None.\"]

## Goal
[What the user is trying to accomplish overall]

## Constraints & Preferences
[User preferences, coding style, constraints, important decisions]

## Completed Actions
[Numbered list of concrete actions taken — include tool used, target, and outcome.
Format each as: N. ACTION target — outcome [tool: name]
Example:
1. READ config.rs:45 — found == should be != [tool: read_file]
2. EDIT config.rs:45 — changed == to != [tool: write_file]
3. BUILD `cargo build` — succeeded, 0 errors [tool: execute_cmd]
Be specific with file paths, commands, line numbers, and results.]

## Active State
[Current working state — include:
- Working directory and branch (if applicable)
- Modified/created files with brief note on each
- Build/test status
- Any running processes or servers
- Environment details that matter]

## In Progress
[Work currently underway — what was being done when compaction fired]

## Blocked
[Any blockers, errors, or issues not yet resolved. Include exact error messages.]

## Key Decisions
[Important technical decisions and WHY they were made]

## Resolved Questions
[Questions the user asked that were ALREADY answered — include the answer so it is not repeated]

## Pending User Asks
[Questions or requests from the user that have NOT yet been answered or fulfilled. If none, write \"None.\"]

## Relevant Files
[Files read, modified, or created — with brief note on each]

## Remaining Work
[What remains to be done — framed as context, not instructions]

## Critical Context
[Any specific values, error messages, configuration details, or data that would \
be lost without explicit preservation. NEVER include API keys, tokens, passwords, \
or credentials — write [REDACTED] instead.]

Write only the summary body. Do not include any preamble or prefix.";

/// How the summariser is asked. Override to change the wording or the sections
/// without touching the mechanics.
pub trait CompactionPrompt: Send + Sync {
    /// The single user message sent to the summariser. `prior` is the previous
    /// summary's body (without [`SUMMARY_PREFIX`]) when this is an update, so
    /// summaries never nest.
    fn build(&self, transcript: &str, prior: Option<&str>) -> String;
}

/// The shipped prompt: preamble + transcript + template, in an update or a
/// first-time shape.
pub struct DefaultPrompt;

impl CompactionPrompt for DefaultPrompt {
    fn build(&self, transcript: &str, prior: Option<&str>) -> String {
        match prior {
            Some(prev) => format!(
                "{SUMMARIZER_PREAMBLE}\n\n\
                 You are updating a context compaction summary. A previous compaction produced \
                 the summary below. New conversation turns have occurred since then and need \
                 to be incorporated.\n\n\
                 PREVIOUS SUMMARY:\n{prev}\n\n\
                 NEW TURNS TO INCORPORATE:\n{transcript}\n\n\
                 Update the summary using this exact structure. PRESERVE all existing information \
                 that is still relevant. ADD new completed actions to the numbered list (continue \
                 numbering). Move items from \"In Progress\" to \"Completed Actions\" when done. \
                 Move answered questions to \"Resolved Questions\". Update \"Active State\" to \
                 reflect current state. Remove information only if it is clearly obsolete. \
                 CRITICAL: Update \"## Active Task\" to reflect the user's most recent unfulfilled \
                 request — this is the most important field for task continuity.\n\n\
                 {SUMMARY_TEMPLATE}"
            ),
            None => format!(
                "{SUMMARIZER_PREAMBLE}\n\n\
                 Create a structured checkpoint summary for the conversation after earlier turns \
                 are compacted. The summary should preserve enough detail for continuity without \
                 re-reading the original turns.\n\n\
                 TURNS TO SUMMARIZE:\n{transcript}\n\n\
                 Use this exact structure:\n\n\
                 {SUMMARY_TEMPLATE}"
            ),
        }
    }
}

// ── Mode / outcome ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum CompactionMode {
    /// Summarise everything except the last `keep_tail` messages, cutting on a
    /// user/agent boundary so an assistant turn is never split from its tool
    /// results.
    Auto { keep_tail: usize },
    /// Summarise up to an explicit message (a UI that lets the user pick).
    UpTo(MessageId),
}

impl Default for CompactionMode {
    fn default() -> Self {
        Self::Auto { keep_tail: 6 }
    }
}

#[derive(Debug, Clone)]
pub struct CompactionOutcome {
    pub summary_id:       SummaryId,
    pub covered_up_to:    MessageId,
    /// The first message the summary does NOT cover — what anything pinned to
    /// a compacted message must be re-anchored onto.
    pub first_surviving:  MessageId,
    pub summary_text:     String,
    pub messages_covered: usize,
    pub usage:            Usage,
}

/// Is it time? `usage` is the previous turn's reported input tokens; when the
/// provider reported none, `estimated` (the host's own count) decides.
pub fn should_compact(usage: Option<u32>, estimated: u32, threshold: u32) -> bool {
    usage.filter(|t| *t > 0).unwrap_or(estimated) >= threshold
}

// ── Compaction ───────────────────────────────────────────────────────────────

/// One compaction, ready to run. Built via
/// [`LoopManager::new_compaction`](crate::manager::LoopManager::new_compaction)
/// so it shares the manager's store, hooks and event bus.
pub struct Compaction {
    pub(crate) store:        Arc<dyn HistoryStore>,
    pub(crate) selector:     Arc<dyn ModelSelector>,
    pub(crate) hooks:        Vec<Arc<dyn LoopHooks>>,
    pub(crate) events:       EventSink,
    pub(crate) conversation: ConversationId,
    pub(crate) frame:        FrameId,
    pub(crate) mode:         CompactionMode,
    pub(crate) hint:         ModelHint,
    pub(crate) prompt:       Arc<dyn CompactionPrompt>,
    pub(crate) temperature:  Option<f32>,
    /// Host free-form, forwarded on the request (payload logging).
    pub(crate) log:          Option<Value>,
}

impl Compaction {
    pub fn mode(mut self, mode: CompactionMode) -> Self {
        self.mode = mode;
        self
    }

    /// Pin the summariser's model. Default: whatever the selector picks.
    pub fn model(mut self, hint: ModelHint) -> Self {
        self.hint = hint;
        self
    }

    /// Override the selector for this call (a cheaper tier, say).
    pub fn selector(mut self, selector: Arc<dyn ModelSelector>) -> Self {
        self.selector = selector;
        self
    }

    pub fn prompt(mut self, prompt: Arc<dyn CompactionPrompt>) -> Self {
        self.prompt = prompt;
        self
    }

    pub fn log(mut self, log: Value) -> Self {
        self.log = Some(log);
        self
    }

    /// Summarise and save. `Ok(None)` means there was nothing worth compacting
    /// — not an error: too few messages, no clean split point, or a summariser
    /// that came back empty.
    pub async fn run(&self) -> crate::Result<Option<CompactionOutcome>> {
        let prior = self.store.latest_summary(self.frame).await?;
        let messages = match &prior {
            Some(s) => self.store.load_since(self.frame, s.covered_up_to).await?,
            None    => self.store.load(self.frame).await?,
        };

        let Some(split) = self.split_point(&messages) else {
            debug!(frame = %self.frame, "compaction: nothing to summarise");
            return Ok(None);
        };
        let (to_summarise, surviving) = messages.split_at(split);
        let covered_up_to = to_summarise.last().expect("split > 0").id;
        let first_surviving = surviving.first().expect("split < len").id;

        let transcript = transcript(to_summarise);
        let body = self.prompt.build(&transcript, prior.as_ref().map(|s| s.text.as_str()));

        let handle = self.selector.select(&self.hint, &[]).await?;
        info!(
            frame = %self.frame,
            model = %handle.id,
            messages = to_summarise.len(),
            "compaction: summarising"
        );
        let request = ModelRequest {
            messages:     vec![json!({ "role": "user", "content": body })],
            tools:        Vec::new(),
            model:        handle.wire_model().to_string(),
            max_tokens:   None,
            temperature:  self.temperature,
            request_id:   uuid_like(),
            conversation: self.conversation.clone(),
            frame:        self.frame,
            extras:       handle.info.extras.clone(),
            log:          self.log.clone(),
        };
        let response = handle.model.complete(&request, None).await.map_err(|e| {
            warn!(frame = %self.frame, error = %e, "compaction: the summariser failed");
            anyhow::anyhow!("compaction: {e}")
        })?;

        let (summary_text, usage) = match response {
            ModelResponse::Message { content, usage, .. } => (content, usage),
            // A summariser has no tools; if one hallucinates a call, its text is
            // still the summary.
            ModelResponse::ToolCalls { content, usage, .. } => {
                warn!(frame = %self.frame, "compaction: unexpected tool calls, using the content");
                (content, usage)
            }
        };
        if summary_text.trim().is_empty() {
            warn!(frame = %self.frame, "compaction: empty summary, nothing saved");
            return Ok(None);
        }

        let summary_id = self
            .store
            .save_summary(self.frame, NewSummary { text: summary_text.clone(), covered_up_to })
            .await?;

        self.events.emit(self.frame, None, LoopEvent::Compacted {
            frame: self.frame,
            covered_up_to,
        });
        for h in &self.hooks {
            h.on_compacted(self.frame, covered_up_to, first_surviving).await;
        }

        info!(frame = %self.frame, %summary_id, %covered_up_to, "compaction: summary saved");
        Ok(Some(CompactionOutcome {
            summary_id,
            covered_up_to,
            first_surviving,
            summary_text,
            messages_covered: to_summarise.len(),
            usage,
        }))
    }

    /// Where to cut. Never between an assistant message and its tool results —
    /// the surviving half would be a tool result answering a call the model
    /// cannot see, which strict APIs reject outright.
    fn split_point(&self, messages: &[StoredMessage]) -> Option<usize> {
        match self.mode {
            CompactionMode::UpTo(id) => {
                let idx = messages.iter().position(|m| m.id == id)? + 1;
                (idx < messages.len()).then_some(idx)
            }
            CompactionMode::Auto { keep_tail } => {
                if messages.len() <= keep_tail {
                    return None;
                }
                let raw = messages.len() - keep_tail;
                let split = (0..=raw)
                    .rev()
                    .find(|&i| i == 0 || matches!(messages[i].role, Role::User | Role::Agent))
                    .unwrap_or(0);
                (split > 0).then_some(split)
            }
        }
    }
}

// ── Transcript ───────────────────────────────────────────────────────────────

/// Head+tail truncation: a summariser needs both how a long output started and
/// how it ended; a prefix cut throws the conclusion away.
fn truncate_head_tail(s: &str, head_chars: usize, tail_chars: usize) -> String {
    let s = s.trim();
    let char_count = s.chars().count();
    if char_count <= head_chars + tail_chars {
        return s.to_string();
    }
    let head_end = s.char_indices().nth(head_chars).map(|(i, _)| i).unwrap_or(s.len());
    let tail_start = s
        .char_indices()
        .nth(char_count - tail_chars)
        .map(|(i, _)| i)
        .unwrap_or(0);
    format!("{}\n...[truncated]...\n{}", &s[..head_end], &s[tail_start..])
}

fn truncate(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let end = s.char_indices().nth(max_chars).map(|(i, _)| i).unwrap_or(s.len());
    format!("{}…", &s[..end])
}

/// The messages as labeled text. Not the wire projection: a summariser reads
/// better prose than JSON, and tool results are worth more than tool schemas.
fn transcript(messages: &[StoredMessage]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in messages {
        match msg.role {
            Role::User | Role::Agent => {
                parts.push(format!("[USER]: {}", truncate_head_tail(&msg.content, 6000, 1500)));
            }
            Role::Assistant => {
                let mut content = truncate_head_tail(&msg.content, 6000, 1500);
                if !msg.calls.is_empty() {
                    let lines: Vec<String> = msg
                        .calls
                        .iter()
                        .map(|c| {
                            let args = c
                                .arguments_raw
                                .clone()
                                .unwrap_or_else(|| c.arguments.to_string());
                            format!("  {}({})", c.name, truncate(&args, 1200))
                        })
                        .collect();
                    content.push_str(&format!("\n[Tool calls:\n{}\n]", lines.join("\n")));
                }
                parts.push(format!("[ASSISTANT]: {content}"));

                for call in &msg.calls {
                    let result = match call.state {
                        CallState::Done => call
                            .result
                            .as_deref()
                            .map(|r| truncate_head_tail(r, 4000, 1500))
                            .unwrap_or_default(),
                        _ => "(failed or interrupted)".to_string(),
                    };
                    parts.push(format!("[TOOL RESULT tc_{}]: {result}", call.id));
                }
            }
            // System messages are built per turn, never stored (see `store`).
            Role::System => {}
        }
    }
    parts.join("\n\n")
}

/// Correlation id for the summariser call (the crate carries no uuid crate).
fn uuid_like() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("compaction-{nanos:032x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{CallOutcome, NewCall, NewMessage};
    use crate::store_memory::InMemoryStore;
    use crate::tool::ToolOutput;

    #[test]
    fn the_threshold_falls_back_to_the_estimate_when_usage_is_missing() {
        assert!(should_compact(Some(120), 0, 100));
        assert!(!should_compact(Some(80), 999, 100));
        // No usage reported (or zero) → the host's own estimate decides.
        assert!(should_compact(None, 120, 100));
        assert!(should_compact(Some(0), 120, 100));
        assert!(!should_compact(None, 80, 100));
    }

    async fn seeded() -> (Arc<InMemoryStore>, FrameId, Vec<StoredMessage>) {
        let store = Arc::new(InMemoryStore::new());
        let conv = ConversationId::new("c");
        let frame = store
            .open_frame(&conv, None, crate::store::FrameSpec::root("a"))
            .await
            .unwrap();
        for i in 0..4 {
            store.append(frame, NewMessage::user(format!("q{i}"))).await.unwrap();
            let m = store
                .append(frame, NewMessage::assistant(format!("a{i}"), None))
                .await
                .unwrap();
            let c = store
                .append_call(m, NewCall::new("read_file", json!({ "path": "x" })))
                .await
                .unwrap();
            store
                .resolve_call(c, &CallOutcome::Completed(ToolOutput::Text("body".into())))
                .await
                .unwrap();
        }
        let msgs = store.load(frame).await.unwrap();
        (store, frame, msgs)
    }

    fn compaction(store: Arc<InMemoryStore>, frame: FrameId, mode: CompactionMode) -> Compaction {
        let (bus, _) = tokio::sync::broadcast::channel(16);
        Compaction {
            store,
            // The split-point tests never reach the model.
            selector:     Arc::new(crate::model::SingleModel::new(crate::testing::FakeModel::new(
                "unused",
                Vec::new(),
            ))),
            hooks:        Vec::new(),
            events:       EventSink::new(ConversationId::new("c"), bus),
            conversation: ConversationId::new("c"),
            frame,
            mode,
            hint:         ModelHint::default(),
            prompt:       Arc::new(DefaultPrompt),
            temperature:  None,
            log:          None,
        }
    }

    #[tokio::test]
    async fn the_cut_never_splits_an_assistant_turn_from_its_tool_results() {
        let (store, frame, msgs) = seeded().await;
        // 8 messages: user/assistant × 4. keep_tail = 3 would cut at index 5 —
        // an assistant message — so it must walk back to the user before it.
        let c = compaction(store, frame, CompactionMode::Auto { keep_tail: 3 });
        let split = c.split_point(&msgs).unwrap();
        assert!(matches!(msgs[split].role, Role::User), "cut at {split}: {:?}", msgs[split].role);
    }

    #[tokio::test]
    async fn there_is_nothing_to_compact_in_a_short_conversation() {
        let (store, frame, msgs) = seeded().await;
        let c = compaction(store, frame, CompactionMode::Auto { keep_tail: 99 });
        assert!(c.split_point(&msgs).is_none());
    }

    #[tokio::test]
    async fn an_explicit_cut_point_covers_it_and_keeps_the_rest() {
        let (store, frame, msgs) = seeded().await;
        let c = compaction(store.clone(), frame, CompactionMode::UpTo(msgs[2].id));
        assert_eq!(c.split_point(&msgs), Some(3));
        // Cutting at the very last message would leave nothing surviving.
        let c = compaction(store, frame, CompactionMode::UpTo(msgs.last().unwrap().id));
        assert_eq!(c.split_point(&msgs), None);
    }

    #[tokio::test]
    async fn the_transcript_carries_calls_and_their_results() {
        let (_store, _frame, msgs) = seeded().await;
        let text = transcript(&msgs[..2]);
        assert!(text.contains("[USER]: q0"), "{text}");
        assert!(text.contains("[ASSISTANT]: a0"), "{text}");
        assert!(text.contains("read_file({\"path\":\"x\"})"), "{text}");
        assert!(text.contains("[TOOL RESULT tc_1]: body"), "{text}");
    }
}
