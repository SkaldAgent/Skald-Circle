//! Context compaction — reduces LLM context size by summarising old messages.
//!
//! # Responsibility
//! [`ContextCompactor`] is a stateless service (all state lives in the DB).
//! It is shared via `Arc` across all [`ChatSessionHandler`]s.
//!
//! It is triggered **at the start of a turn** when the previous turn's
//! `input_tokens` exceeds the configured threshold (Opzione C from the design
//! doc), or manually via `force_compact`.  Ephemeral sessions (cron, tic)
//! are always skipped.
//!
//! # Compaction flow
//! ```text
//! handle_message()
//!   └─► ContextCompactor::try_compact(pool, stack_id, last_input_tokens)
//!         │
//!         ├─ guard: tokens < threshold → return Ok(false)
//!         ├─ guard: is_ephemeral       → return Ok(false)
//!         │
//!         └─► do_compact(pool, session_id, stack_id, effective_tokens)
//!               ├─ load latest summary (if any)
//!               ├─ load raw messages since last summary boundary
//!               │    (or all messages if no prior summary)
//!               ├─ split:  to_summarise  = messages[0 .. len - keep_recent]
//!               │          to_keep_raw   = messages[len - keep_recent ..]
//!               ├─ if to_summarise is empty → return Ok(false)
//!               ├─ build compaction prompt (system hard-coded + user = conversation text)
//!               ├─ call LLM (no tools, strength-based AUTO selection)
//!               ├─ save summary to chat_summaries
//!               └─ publish BusEvent::CompactionDone
//!
//! force_compact() skips the threshold guard and calls do_compact() directly.
//! ```
//!
//! # build_openai_messages after compaction
//! ```text
//! latest_summary = chat_summaries::latest_for_stack(pool, stack_id)
//! if let Some(s) = latest_summary:
//!     inject <summary>…</summary> after system prompt
//!     load messages with id > s.covers_up_to_message_id
//! else:
//!     load all messages (current behaviour)
//! apply max_history_messages drain as safety floor (only when compaction is disabled)
//! ```

use std::sync::Arc;

use serde_json::json;
use sqlx::SqlitePool;
use tracing::{debug, info, warn};

use crate::core::chat_event_bus::{ChatEventBus, CompactionEvent};
use crate::core::chatbot::ChatOptions;
use crate::core::config::CompactionConfig;
use crate::core::db::{chat_history, chat_llm_tools, chat_summaries};
use crate::core::llm::LlmManager;

// ── Compaction constants (ported from Hermes context_compressor.py) ──────────
//
// SUMMARY_PREFIX  — prepended to every stored summary when injected as context.
//                   Tells the LLM this is historical reference, not live instructions.
// SUMMARIZER_PREAMBLE — system/user-message preamble for the summarisation LLM call.
// SUMMARY_TEMPLATE    — structured section template the LLM must follow.

/// Prefix prepended to the summary content when it is injected into the
/// message array as context for the main agent.  Exposed as `pub` so that
/// `build_openai_messages` can use the same wording.
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

/// Preamble shared by both first-compaction and iterative-update prompts.
/// Wording is deliberately plain to avoid content-filter false positives.
const SUMMARIZER_PREAMBLE: &str = "\
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

/// Structured section template the summariser must fill in.
const SUMMARY_TEMPLATE: &str = "\
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

// ── Public API ────────────────────────────────────────────────────────────────

pub struct ContextCompactor {
    config:      CompactionConfig,
    llm_manager: Arc<LlmManager>,
    event_bus:   Arc<ChatEventBus>,
}

impl ContextCompactor {
    pub fn new(
        config:      CompactionConfig,
        llm_manager: Arc<LlmManager>,
        event_bus:   Arc<ChatEventBus>,
    ) -> Self {
        Self { config, llm_manager, event_bus }
    }

    /// Attempt to compact the conversation history for `stack_id`.
    ///
    /// * `last_input_tokens` — input tokens from the **previous** turn.
    ///   Pass `0` when the provider did not report usage (a character-count
    ///   estimate is used as fallback in that case).
    /// * `is_ephemeral` — skip compaction for short-lived automated sessions.
    ///
    /// Returns `true` if a new summary was written, `false` if skipped.
    pub async fn try_compact(
        &self,
        pool:              &SqlitePool,
        session_id:        i64,
        stack_id:          i64,
        last_input_tokens: u32,
        is_ephemeral:      bool,
    ) -> anyhow::Result<bool> {
        if is_ephemeral {
            return Ok(false);
        }

        let effective_tokens = if last_input_tokens > 0 {
            last_input_tokens
        } else {
            let est = chat_history::estimate_tokens_for_stack(pool, stack_id).await?;
            debug!(stack_id, estimate = est, "compactor: no usage data, using char estimate");
            est
        };

        if effective_tokens < self.config.threshold_tokens {
            return Ok(false);
        }

        info!(
            stack_id,
            effective_tokens,
            threshold = self.config.threshold_tokens,
            "compactor: threshold exceeded, starting compaction"
        );

        self.do_compact(pool, session_id, stack_id, effective_tokens).await
    }

    /// Force compaction regardless of the token threshold.
    /// Still respects the ephemeral guard.
    ///
    /// Returns `true` if a new summary was written, `false` if skipped.
    pub async fn force_compact(
        &self,
        pool:         &SqlitePool,
        session_id:   i64,
        stack_id:     i64,
        is_ephemeral: bool,
    ) -> anyhow::Result<bool> {
        if is_ephemeral {
            return Ok(false);
        }

        let effective_tokens = chat_history::estimate_tokens_for_stack(pool, stack_id).await?;
        info!(
            stack_id,
            effective_tokens,
            "compactor: manual compaction triggered"
        );

        self.do_compact(pool, session_id, stack_id, effective_tokens).await
    }

    /// Core compaction logic shared by `try_compact` and `force_compact`.
    /// Loads messages, splits at the keep_recent boundary, calls the summariser
    /// LLM, persists the summary, and publishes a `CompactionDone` event.
    async fn do_compact(
        &self,
        pool:             &SqlitePool,
        session_id:       i64,
        stack_id:         i64,
        effective_tokens: u32,
    ) -> anyhow::Result<bool> {
        let prior_summary = chat_summaries::latest_for_stack(pool, stack_id).await?;

        let messages = match &prior_summary {
            Some(s) => chat_history::for_stack_since(pool, stack_id, s.covers_up_to_message_id).await?,
            None    => chat_history::for_stack(pool, stack_id).await?,
        };

        let keep = self.config.keep_recent;

        if messages.len() <= keep {
            debug!(
                stack_id,
                messages = messages.len(),
                keep,
                "compactor: not enough messages to summarise beyond keep_recent, skipping"
            );
            return Ok(false);
        }

        let raw_split = messages.len() - keep;
        let split = (0..=raw_split)
            .rev()
            .find(|&i| {
                i == 0 || matches!(
                    messages[i].role,
                    chat_history::Role::User | chat_history::Role::Agent
                )
            })
            .unwrap_or(0);

        if split == 0 {
            debug!(stack_id, "compactor: no suitable split point found, skipping");
            return Ok(false);
        }

        let to_summarise = &messages[..split];
        let last_covered_id = to_summarise.last().expect("to_summarise is non-empty").id;

        let conversation_text = self
            .format_for_summary(pool, to_summarise, prior_summary.as_ref().map(|s| s.content.as_str()))
            .await?;

        let (client_name, llm) = self.llm_manager
            .resolve(None, None, self.config.strength)
            .await?;

        info!(
            stack_id,
            client = %client_name,
            messages_covered = to_summarise.len(),
            last_covered_id,
            "compactor: calling LLM for summary"
        );

        let messages_payload = vec![
            json!({ "role": "user", "content": conversation_text }),
        ];

        let options = ChatOptions {
            model:       llm.model.clone(),
            max_tokens:  None,
            temperature: Some(0.3),
            session_id:  Some(session_id),
            stack_id:    Some(stack_id),
        };

        let turn = llm.client.chat_with_tools(&messages_payload, &[], &options).await
            .map_err(|e| {
                warn!(stack_id, error = %e, "compactor: LLM call failed");
                e
            })?;

        let summary_text = match turn {
            crate::core::chatbot::LlmTurn::Message(resp) => resp.content,
            crate::core::chatbot::LlmTurn::ToolCalls { content, .. } => {
                warn!(stack_id, "compactor: unexpected tool calls in summary response, using content");
                content
            }
        };

        if summary_text.trim().is_empty() {
            warn!(stack_id, "compactor: LLM returned empty summary, skipping save");
            return Ok(false);
        }

        let summary_id = chat_summaries::save(pool, stack_id, &summary_text, last_covered_id).await?;

        info!(
            stack_id,
            summary_id,
            last_covered_id,
            "compactor: summary saved"
        );

        self.event_bus.compaction_done(CompactionEvent {
            session_id,
            stack_id,
            summary_id,
            covers_up_to_message_id: last_covered_id,
            triggered_by_tokens: effective_tokens,
        });

        Ok(true)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Builds the full prompt for the summarisation LLM call (Hermes-style).
    ///
    /// Returns a single string intended to be sent as a `user` message.
    /// The preamble, conversation transcript, and structured template are all
    /// concatenated, matching how Hermes' `_generate_summary` works.
    ///
    /// * First compaction  — `prior_summary` is `None`.
    /// * Subsequent compaction — `prior_summary` contains the previous summary body
    ///   (without `SUMMARY_PREFIX`) so the LLM can produce an updated, non-nested summary.
    async fn format_for_summary(
        &self,
        pool:          &SqlitePool,
        messages:      &[chat_history::ChatMessage],
        prior_summary: Option<&str>,
    ) -> anyhow::Result<String> {
        let transcript = self.serialize_for_summary(pool, messages).await?;

        let prompt = if let Some(prev) = prior_summary {
            format!(
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
            )
        } else {
            format!(
                "{SUMMARIZER_PREAMBLE}\n\n\
                 Create a structured checkpoint summary for the conversation after earlier turns \
                 are compacted. The summary should preserve enough detail for continuity without \
                 re-reading the original turns.\n\n\
                 TURNS TO SUMMARIZE:\n{transcript}\n\n\
                 Use this exact structure:\n\n\
                 {SUMMARY_TEMPLATE}"
            )
        };

        Ok(prompt)
    }

    /// Serialises conversation messages into Hermes-style labeled text for the summariser.
    ///
    /// Format:
    /// ```text
    /// [USER]: text…
    ///
    /// [ASSISTANT]: text…
    /// [Tool calls:
    ///   tool_name(args…)
    /// ]
    ///
    /// [TOOL RESULT tc_N]: result…
    /// ```
    ///
    /// Long content is truncated with a head+tail strategy (preserving the start and
    /// end of the text) rather than a simple prefix cut.
    async fn serialize_for_summary(
        &self,
        pool:     &SqlitePool,
        messages: &[chat_history::ChatMessage],
    ) -> anyhow::Result<String> {
        let mut parts: Vec<String> = Vec::new();

        for msg in messages {
            match msg.role {
                chat_history::Role::User | chat_history::Role::Agent => {
                    let content = truncate_head_tail(msg.content.trim(), 6000, 1500);
                    parts.push(format!("[USER]: {content}"));
                }
                chat_history::Role::Assistant => {
                    let mut content = truncate_head_tail(msg.content.trim(), 6000, 1500);

                    let tool_calls = chat_llm_tools::for_message(pool, msg.id).await?;

                    if !tool_calls.is_empty() {
                        let tc_lines: String = tool_calls
                            .iter()
                            .map(|tc| {
                                let args = tc.arguments.as_deref()
                                    .map(|a| truncate(a, 1200))
                                    .unwrap_or_default();
                                format!("  {}({})", tc.name, args)
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        content.push_str(&format!("\n[Tool calls:\n{tc_lines}\n]"));
                    }

                    parts.push(format!("[ASSISTANT]: {content}"));

                    // Tool results as separate labeled entries — mirrors Hermes'
                    // `[TOOL RESULT {call_id}]` entries in the serialised transcript.
                    for tc in &tool_calls {
                        let result = match tc.status.as_str() {
                            "done" => tc.result.as_deref()
                                .map(|r| truncate_head_tail(r, 4000, 1500))
                                .unwrap_or_default(),
                            _ => "(failed or interrupted)".to_string(),
                        };
                        parts.push(format!("[TOOL RESULT tc_{}]: {result}", tc.id));
                    }
                }
            }
        }

        Ok(parts.join("\n\n"))
    }
}

/// Truncate a string to at most `max_chars`, appending "…" if truncated.
fn truncate(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let end = s.char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        format!("{}…", &s[..end])
    }
}

/// Keep the first `head_chars` and last `tail_chars` of a string, inserting
/// `\n...[truncated]...\n` in the middle when the string is longer than their sum.
///
/// Mirrors Hermes' `_CONTENT_HEAD` + `_CONTENT_TAIL` strategy so the summariser
/// always sees both the beginning context and the ending result of verbose outputs.
fn truncate_head_tail(s: &str, head_chars: usize, tail_chars: usize) -> String {
    let s = s.trim();
    let char_count = s.chars().count();
    let total = head_chars + tail_chars;
    if char_count <= total {
        return s.to_string();
    }
    let head_end = s.char_indices()
        .nth(head_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    let tail_start = s.char_indices()
        .nth(char_count - tail_chars)
        .map(|(i, _)| i)
        .unwrap_or(0);
    format!("{}\n...[truncated]...\n{}", &s[..head_end], &s[tail_start..])
}
