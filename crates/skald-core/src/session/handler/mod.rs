use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use tracing::{error, info, trace, warn};

use crate::approval::ApprovalManager;
use crate::run_context::RunContext;
use crate::tools::tool_names as tn;
use crate::chat_event_bus::{ChatEvent, ChatEventBus, ChatEventRole};
use crate::clarification::ClarificationManager;
use crate::compactor::ContextCompactor;
use crate::config::DatetimeConfig;
use crate::db::{chat_history, chat_sessions_stack};
use crate::events::ServerEvent;
use core_api::message_meta::MessageMetadata;
use core_api::user_fs::SharedFs;
use crate::llm::LlmManager;
use crate::mcp::McpProvider;
use crate::image_generate::ImageGeneratorManager;
use crate::memory::MemoryManager;
use crate::tool_discovery::ToolDiscovery;
use crate::tools::ToolRegistry;

mod approval;
mod agent_dispatch;
mod config;
mod dispatch;
mod emitter;
mod gate;
mod interface_tools;
mod llm_call;
mod llm_loop;
pub mod media;
pub mod message_builder;
mod messages;
mod outcome;
mod resume;

use emitter::TurnEmitter;

pub use interface_tools::{InterfaceTool, ToolFuture};

pub const DEFAULT_MAX_TOOL_ROUNDS: usize = 20;

/// Default maximum number of synchronous sub-agents dispatched concurrently when
/// the LLM emits a homogeneous batch of sub-agent calls in a single response.
/// Bounds fan-out so a large batch does not trigger provider rate-limit storms.
pub const DEFAULT_MAX_PARALLEL_SUBAGENTS: usize = 4;

pub(super) const MAX_AGENT_DEPTH: i64 = 5;

/// A queued user message to be appended to history mid-turn (drained from the
/// source inbox at a round boundary).
pub struct PendingMsg {
    pub content:  String,
    pub metadata: Option<MessageMetadata>,
}

/// Source of queued user input for the in-flight turn. Implemented by `ChatHub`
/// over a source's inbox; it lets `run_agent_turn` pull newly-queued user
/// messages at each round boundary and inject them live into the running turn.
///
/// Passed as `Some` only for the root interactive turn. Sub-agents, resume, and
/// non-interactive runners (cron, TIC) pass `None` — they never inject.
#[async_trait]
pub trait PendingUserInput: Send + Sync {
    /// Drains the leading run of queued non-synthetic user messages, one entry
    /// each. Returns empty when there is nothing to inject.
    async fn drain_user(&self) -> Vec<PendingMsg>;
}

/// Control-flow signals returned as `anyhow::Error` by internal dispatch methods.
/// Using a typed enum instead of two separate sentinel structs allows a single
/// `downcast_ref` in `llm_loop` instead of two separate type checks.
#[derive(Debug)]
pub(super) enum AgentFlowSignal {
    /// The WS disconnected while `dispatch_ask_user_clarification` was blocking.
    /// The tool stays `'pending'` in DB so `resume_pending_tools` can re-ask on reconnect.
    QuestionChannelClosed,
}

impl std::fmt::Display for AgentFlowSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QuestionChannelClosed => write!(f, "question channel closed (WS disconnected)"),
        }
    }
}

impl std::error::Error for AgentFlowSignal {}

pub(super) enum TurnOutcome {
    Final {
        content:       String,
        message_id:    i64,
        input_tokens:  Option<u32>,
        output_tokens: Option<u32>,
        truncated:     bool,
        /// All tool calls executed during this turn, across all rounds.
        tool_calls:    Vec<crate::chat_event_bus::ToolCallEvent>,
    },
    Cancelled,
    Exhausted,
}

/// Truncate `s` to at most `max_chars` characters, appending `…` when it was
/// longer. Char-boundary safe: a raw `&s[..n]` byte slice panics when byte `n`
/// lands inside a multi-byte UTF-8 character (e.g. an em-dash or emoji straddling
/// the cut point), which is exactly how a well-formed sub-agent result once
/// unwound a whole turn. Used for every event/log preview.
pub(super) fn preview_truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => format!("{}…", &s[..byte_idx]),
        None                => s.to_string(),
    }
}

pub(super) fn update_scratchpad_tool_def() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tn::UPDATE_SCRATCHPAD,
            "description": "Write or update a key-value note in the session scratchpad. \
                            Notes are shared by all agents in this chat session and automatically \
                            injected into every agent's context. Not persisted across sessions. \
                            Use it for temporary discoveries: architecture notes, path lookups, \
                            decisions that other agents in this session need to know about.",
            "parameters": {
                "type": "object",
                "properties": {
                    "key":   { "type": "string", "description": "Short identifier for this note (e.g. 'db_url', 'main_struct')." },
                    "value": { "type": "string", "description": "Content of the note." }
                },
                "required": ["key", "value"]
            }
        }
    })
}

/// Tool definition for `write_todos` — a private, per-turn task list the agent
/// uses to plan and track its own progress.
///
/// Unlike `update_scratchpad` (a shared blackboard injected into every agent in
/// the session), `write_todos` is **stateless**: the list lives only in this
/// agent's own tool-result history. Because conversation history is per-stack,
/// it is never visible to sub-agents or to the caller — no DB storage needed.
/// The agent re-sends the whole list (TodoWrite-style) on every update.
pub(super) fn write_todos_tool_def() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tn::WRITE_TODOS,
            "description": "Record and update your task list for the current turn, to plan multi-step \
                            work and track progress. Re-send the ENTIRE list on every call (including \
                            already-completed items with their new status) — this replaces the previous \
                            list. Keep exactly one item `in_progress` at a time. This list is PRIVATE \
                            to you: it is not shared with sub-agents you dispatch, nor returned to your \
                            caller (use `update_scratchpad` instead for notes other agents must see).",
            "parameters": {
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The full, ordered task list. Re-send it entirely on every update.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "Short description of the task." },
                                "status":  { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "Current status of this task." }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }
        }
    })
}

/// Tool definition that lets a sub-agent (depth > 0) dispatch a further
/// synchronous sub-agent. The call is intercepted in `run_agent_turn` and routed
/// to `dispatch_sub_agent` (the InterfaceTool handler is never reached), so only
/// the definition is needed here. `agent_id` is required because
/// `dispatch_sub_agent` rejects calls without it.
fn execute_subtask_tool_def() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tn::EXECUTE_SUBTASK,
            "description": "Delegate work to another agent and get its result. Runs the \
                            named agent synchronously with the given prompt and blocks until \
                            it finishes, returning its final answer as the tool result. Use \
                            `list_agents` first to see which agents are available.",
            "parameters": {
                "type": "object",
                "properties": {
                    "agent_id":    { "type": "string", "description": "Id of the agent to run (see `list_agents`)." },
                    "title":       { "type": "string", "description": "Short name for this sub-task." },
                    "description": { "type": "string", "description": "What this sub-task does." },
                    "prompt":      { "type": "string", "description": "Prompt sent to the agent." }
                },
                "required": ["agent_id", "prompt"]
            }
        }
    })
}

fn ask_user_clarification_tool_def() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tn::ASK_USER_CLARIFICATION,
            "description": "Pause execution and ask the user a clarification question. \
                            Use when requirements are ambiguous, a dependency is missing, \
                            or a decision requires user input before continuing. \
                            The user's answer is returned as the tool result.",
            "parameters": {
                "type": "object",
                "properties": {
                    "title":    { "type": "string", "description": "Short label shown in the inbox card (e.g. 'Missing API key')." },
                    "question": { "type": "string", "description": "Full question text." },
                    "suggested_answers": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of suggested answers shown as chips. The user can pick one or type freely."
                    }
                },
                "required": ["title", "question"]
            }
        }
    })
}


pub enum ApprovalDecision {
    Approved,
    Rejected { note: String },
}

impl ApprovalDecision {
    /// Canonical tool-result text shown to the LLM for a human rejection,
    /// given the raw user-supplied note (which may be empty). This is the
    /// single source of truth: every reject path passes the raw note and lets
    /// this build the message, so the wording stays consistent and the note
    /// carries the user's justification verbatim — no surface-specific prefixes.
    pub fn rejection_message(note: &str) -> String {
        let note = note.trim();
        if note.is_empty() {
            "User rejected this tool call.".to_string()
        } else {
            format!("User rejected this tool call. Reason: {note}")
        }
    }
}

pub struct ChatSessionHandler {
    pub session_id:              i64,
    pub(super) db:               Arc<SqlitePool>,
    /// The shared (`system.db`) pool. Owner-bound work uses `db`; this is only for
    /// cross-owner reads, e.g. injecting `shared-memory/` notes into the prompt.
    pub(super) shared_pool:      Arc<SqlitePool>,
    /// The authenticated user who owns this session. Threaded into `ChatOptions`
    /// so the telemetry metadata row in `system.db` carries `user_id`.
    pub(super) user_id:          String,
    /// The owner's filesystem view (home + shared folders + container), threaded
    /// into every [`ToolContext`] so disk fs-tools resolve per-user host paths and
    /// `execute_cmd` execs into the owner's container (blueprint §6). A **shared
    /// swappable cell** (not a snapshot): a shared-folder membership change is
    /// applied in place (§6 remount), so a live session picks it up on its next
    /// tool call without being rebuilt — see [`SharedFs`].
    pub(super) fs:               SharedFs,
    pub(super) llm_manager:      Arc<LlmManager>,
    pub(super) max_history_messages:  usize,
    pub(super) max_tool_rounds:       usize,
    /// Max synchronous sub-agents dispatched concurrently for a homogeneous batch
    /// of sub-agent calls in a single LLM response (`1` = sequential).
    pub(super) max_parallel_subagents: usize,
    /// If `Some(n)`, tool results from previous turns that exceed `n` characters
    /// are replaced with a placeholder when building the LLM context.
    /// The database always retains the original content.
    pub(super) max_tool_result_chars: Option<usize>,
    pub(super) datetime_config:       DatetimeConfig,
    pub(super) agent_id:         String,
    /// Source of the session: "web", "telegram", "cron", etc.
    pub(super) source:           String,
    /// True when a real user is actively participating (web, telegram).
    pub(super) is_interactive:   bool,
    /// True for short-lived automated sessions (cron, tic).
    pub(super) is_ephemeral:     bool,
    pub(super) tools:            Arc<ToolRegistry>,
    pub(super) mcp:              Arc<dyn McpProvider>,
    /// Records tools offered to the LLM each round so the Security-groups UI can
    /// list/gate dynamically-injected tools (interface/plugin/provider tools).
    pub(super) tool_discovery:   Arc<ToolDiscovery>,
    pub(super) approval:         Arc<ApprovalManager>,
    pub(super) clarification:    Arc<ClarificationManager>,
    pub(super) event_bus:        Arc<ChatEventBus>,
    /// Human-readable label injected by background runners (e.g. "CronJob: Daily Digest").
    pub(super) context_label:    std::sync::RwLock<Option<String>>,
    pub(super) memory_manager:         Arc<MemoryManager>,
    pub(super) image_generator_manager: Arc<ImageGeneratorManager>,
    /// Prevents concurrent handle_message calls on the same session.
    pub(super) processing:       Mutex<()>,
    /// Cancellation scope for the in-flight turn. A fresh token is minted per
    /// user message (`handle_message`) and per resume (`resume_turn`), then a
    /// clone is threaded by value through the whole (possibly recursive) call
    /// tree. `cancel()` cancels whatever token is currently stored, which the
    /// running chain observes because it holds its own clone of that same token.
    /// Replacing the field only affects the *next* turn — that is what makes a
    /// stop sticky across sub-agent recursion (it is never reset mid-turn).
    pub(super) current_cancel:   std::sync::Mutex<CancellationToken>,
    /// When true, any tool call that would require human approval is automatically
    /// denied instead of blocking. Used by TicManager and other headless runners
    /// that cannot process approval requests.
    pub(super) auto_deny_approvals: AtomicBool,
    /// Tool-call ids the user already approved via a resolve endpoint after a restart
    /// (no live oneshot to unblock). The next resume's approval gate skips re-gating
    /// these so a post-restart approve dispatches the tool without a second prompt.
    pub(super) pre_approved:     std::sync::Mutex<std::collections::HashSet<i64>>,
    /// Context compactor, shared across all sessions.  `None` when compaction
    /// is disabled (no `compaction` section in config).
    pub(super) compactor:        Option<Arc<ContextCompactor>>,
    /// Input token count from the most recently completed turn, stored
    /// atomically so the next `handle_message` call can decide whether to
    /// compact before processing the new message.  Zero means unknown
    /// (provider did not report usage on the first turn).
    pub(super) last_input_tokens: AtomicU32,
    /// Active RunContext for this session. `None` means the "default" group is used implicitly.
    pub(super) run_context: tokio::sync::RwLock<Option<RunContext>>,
    /// When set, scratchpad reads/writes use this session_id instead of `self.session_id`.
    /// Used by async sub-tasks to share the parent's scratchpad.
    pub(super) scratchpad_session_id: std::sync::OnceLock<i64>,
}

impl ChatSessionHandler {
    pub fn new(
        session_id:            i64,
        db:                    Arc<SqlitePool>,
        shared_pool:           Arc<SqlitePool>,
        user_id:               String,
        fs:                    SharedFs,
        llm_manager:           Arc<LlmManager>,
        max_history_messages:  usize,
        max_tool_rounds:       usize,
        max_parallel_subagents: usize,
        max_tool_result_chars: Option<usize>,
        datetime_config:       DatetimeConfig,
        agent_id:              String,
        source:                String,
        is_interactive:        bool,
        is_ephemeral:          bool,
        tools:                 Arc<ToolRegistry>,
        mcp:                   Arc<dyn McpProvider>,
        approval:              Arc<ApprovalManager>,
        clarification:         Arc<ClarificationManager>,
        event_bus:             Arc<ChatEventBus>,
        memory_manager:           Arc<MemoryManager>,
        image_generator_manager:  Arc<ImageGeneratorManager>,
        compactor:                Option<Arc<ContextCompactor>>,
        run_context:              Option<RunContext>,
        tool_discovery:           Arc<ToolDiscovery>,
    ) -> Self {
        Self {
            session_id,
            db,
            shared_pool,
            user_id,
            fs,
            llm_manager,
            max_history_messages,
            max_tool_rounds,
            max_parallel_subagents,
            max_tool_result_chars,
            datetime_config,
            agent_id,
            source,
            is_interactive,
            is_ephemeral,
            tools,
            mcp,
            tool_discovery,
            approval,
            clarification,
            event_bus,
            memory_manager,
            image_generator_manager,
            compactor,
            context_label:          std::sync::RwLock::new(None),
            processing:             Mutex::new(()),
            current_cancel:         std::sync::Mutex::new(CancellationToken::new()),
            auto_deny_approvals:    AtomicBool::new(false),
            pre_approved:           std::sync::Mutex::new(std::collections::HashSet::new()),
            last_input_tokens:      AtomicU32::new(0),
            run_context:            tokio::sync::RwLock::new(run_context),
            scratchpad_session_id:  std::sync::OnceLock::new(),
        }
    }

    /// Sets the human-readable context label for this session (e.g. "CronJob: Daily Digest").
    /// Called by background runners after the handler is created.
    pub fn set_context_label(&self, label: impl Into<String>) {
        if let Ok(mut g) = self.context_label.write() {
            *g = Some(label.into());
        }
    }

    /// Override the session used for scratchpad reads/writes.
    /// Called by the cron runner for async tasks so they share the parent's scratchpad.
    pub fn set_scratchpad_session_id(&self, id: i64) {
        let _ = self.scratchpad_session_id.set(id);
    }

    /// Returns the session_id to use for scratchpad operations.
    pub(super) fn scratchpad_sid(&self) -> i64 {
        *self.scratchpad_session_id.get().unwrap_or(&self.session_id)
    }

    /// Updates the active RunContext for this session at runtime.
    pub async fn set_run_context(&self, ctx: Option<RunContext>) {
        *self.run_context.write().await = ctx;
    }

    /// Returns the serialised JSON blob of the active RunContext (for storing on child tasks).
    pub async fn run_context_json(&self) -> Option<String> {
        self.run_context.read().await.as_ref().map(|rc| rc.to_db())
    }

    /// Returns the active tool_permission_groups id for approval checks.
    pub(super) async fn tool_group_id(&self) -> Option<String> {
        self.run_context.read().await.as_ref().and_then(|rc| rc.tool_group_id().map(str::to_owned))
    }

    /// Cancels the in-flight turn. The running call tree holds its own clone of
    /// the same token, so it stops at the next round boundary, on the in-flight
    /// LLM call, and on cancellable tools (e.g. `execute_cmd`). Sticky across
    /// sub-agent recursion: the token is never reset mid-turn.
    pub fn cancel(&self) {
        self.current_cancel.lock().unwrap().cancel();
    }

    /// True if a turn is currently in flight (the `processing` mutex is held for
    /// the whole duration of `handle_message` / `resume_turn`). Used to tell a
    /// freshly (re)connected client to show the STOP button.
    pub fn is_processing(&self) -> bool {
        self.processing.try_lock().is_err()
    }

    /// When set, any tool call that would require human approval is automatically
    /// denied instead of blocking indefinitely.
    pub fn set_auto_deny_approvals(&self) {
        self.auto_deny_approvals.store(true, Ordering::Relaxed);
    }

    /// Records that the user already approved this tool_call via a resolve endpoint
    /// after a restart (no live oneshot to unblock). The next resume's approval gate
    /// consumes this and skips re-gating, so the tool dispatches without re-prompting.
    pub fn mark_pre_approved(&self, tool_call_id: i64) {
        self.pre_approved.lock().unwrap().insert(tool_call_id);
    }

    /// Cancels all pending approvals for this session in the ApprovalManager.
    /// Called when the WS connection is lost mid-approval so the waiting future unblocks.
    pub async fn cancel_pending_approvals(&self) {
        self.approval.cancel_for_session(self.session_id).await;
    }

    /// Resolves a pending `ask_user_clarification` call with the user's answer.
    pub async fn resolve_question(&self, request_id: i64, answer: String) {
        if !self.clarification.resolve(request_id, answer).await {
            warn!(session_id = self.session_id, request_id, "resolve_question: request_id not found in ClarificationManager");
        }
    }

    /// Cancels all pending clarification requests for this session (WS disconnected).
    /// The blocked `rx.await` in dispatch_ask_user_clarification returns Err → TurnOutcome::Cancelled,
    /// leaving the tool as 'pending' so resume_pending_tools re-dispatches on reconnect.
    pub async fn cancel_pending_questions(&self) {
        self.clarification.cancel_for_session(self.session_id).await;
    }

    /// Force compaction of the current stack's conversation history.
    /// Bypasses the token threshold check; still respects the ephemeral guard.
    /// Returns `true` if a new summary was written, `false` if skipped.
    pub async fn force_compact(&self) -> anyhow::Result<bool> {
        let pool = &self.db;
        let stack = match chat_sessions_stack::active_for_session(pool, self.session_id).await? {
            Some(s) => s,
            None => return Ok(false),
        };
        match self.compactor {
            Some(ref compactor) => {
                compactor.force_compact(pool, self.session_id, stack.id, self.is_ephemeral).await
            }
            None => Ok(false),
        }
    }

    /// Processes a user message end-to-end:
    /// saves it, runs the tool-calling loop, saves the final response,
    /// sends a Done event. Only one call can run at a time per session.
    pub async fn handle_message(
        &self,
        content:                      &str,
        client_name:                  Option<String>,
        extra_system_context:         Option<String>,
        // Per-turn dynamic system suffix injected AFTER conversation history.
        // Merged with the Honcho memory context (which also lives at position 5).
        // Use for per-turn framing that must not pollute the cacheable static prefix
        // (e.g. notification behavioural instructions from ChatHub).
        extra_system_dynamic_override: Option<String>,
        tail_reminder:                Option<String>,
        interface_tools:              Vec<InterfaceTool>,
        system_substitutions:         HashMap<String, String>,
        tx:                           mpsc::Sender<ServerEvent>,
        // True for system-generated messages injected as user turns
        // (TicManager ticks, notification briefings from ChatHub).
        is_synthetic:                 bool,
        // Structured metadata persisted on the user turn (e.g. file attachments).
        // The MessageBuilder derives the LLM-facing block; the UI renders chips.
        metadata:                     Option<MessageMetadata>,
        // Queued user input for this source. When `Some`, `run_agent_turn` drains
        // it at each round boundary and injects newly-arrived user messages into
        // the running turn. `None` for sub-agents / resume / non-interactive runners.
        pending_input:                Option<Arc<dyn PendingUserInput>>,
    ) -> anyhow::Result<()> {
        let _guard = self.processing.lock().await;
        // Fresh cancellation scope for this user message. Stored so `cancel()`
        // can reach it, and cloned-by-value into the call tree so a /stop during
        // the turn is sticky across sub-agent recursion (never reset mid-turn).
        let token = CancellationToken::new();
        *self.current_cancel.lock().unwrap() = token.clone();
        let pool   = &self.db;
        let em     = TurnEmitter::new(&tx);

        // Retrieve memory context (Honcho or other backend) for this turn.
        // Kept SEPARATE from extra_system_context (the static part) so it can be
        // injected as a dynamic tail system message after the conversation history
        // rather than embedded in the cacheable static prefix.  This allows
        // providers with prefix caching (e.g. Alibaba/DeepSeek via OpenRouter)
        // to cache the stable system prompt across turns even though Honcho
        // memories change on every call.
        let honcho_dynamic = match self.memory_manager.query_context(self.session_id, content).await {
            Some(mem_ctx) => {
                trace!(
                    session_id = self.session_id,
                    chars = mem_ctx.len(),
                    "handle_message: memory context retrieved (will be injected as dynamic tail)"
                );
                Some(mem_ctx)
            }
            None => {
                trace!(
                    session_id = self.session_id,
                    "handle_message: no memory context returned (cold start, unavailable, or nothing to say)"
                );
                None
            }
        };

        // Merge Honcho memories with any per-turn override from the caller.
        // The override goes last so it sits closest to the generation point (recency bias).
        // extra_system_context (passed by the caller) is the STATIC part:
        // interface-specific formatting rules (e.g. Telegram HTML format),
        // never changes turn-to-turn, safe to include in the cached prefix.
        let extra_system_dynamic = match (honcho_dynamic, extra_system_dynamic_override) {
            (Some(honcho), Some(override_)) => Some(format!("{honcho}\n\n{override_}")),
            (Some(honcho), None)            => Some(honcho),
            (None, Some(override_))         => Some(override_),
            (None, None)                    => None,
        };

        let mut config = self.build_agent_config(
            client_name, extra_system_context, extra_system_dynamic, interface_tools, system_substitutions,
        ).await?;
        config.tail_reminder = tail_reminder;

        let stack = match chat_sessions_stack::active_for_session(pool, self.session_id).await? {
            Some(s) => s,
            None    => {
                chat_sessions_stack::create(pool, self.session_id, "main", None, 0, None).await?
            }
        };

        info!(session_id = self.session_id, stack_id = stack.id, client = %config.client_name, "handle_message start");

        // ── Context compaction (Opzione C: at the start of the next turn) ────
        // Check whether the previous turn's input token count exceeded the
        // threshold. If so, summarise the old history before processing the
        // new message.  This keeps latency transparent to the user — the wait
        // happens here, before the LLM loop, and is not a separate turn.
        if let Some(ref compactor) = self.compactor {
            let last_tokens = self.last_input_tokens.load(Ordering::Relaxed);
            match compactor.try_compact(pool, self.session_id, stack.id, last_tokens, self.is_ephemeral).await {
                Ok(true)  => info!(session_id = self.session_id, stack_id = stack.id, "handle_message: context compacted"),
                Ok(false) => {}
                Err(e)    => warn!(session_id = self.session_id, error = %e, "handle_message: compaction failed (non-fatal), continuing"),
            }
        }
        // ─────────────────────────────────────────────────────────────────────

        // If the previous turn was cancelled before the LLM responded, the history ends on a
        // User message with no following assistant. This breaks the user→assistant alternation
        // required by strict APIs (e.g. OpenRouter). Mark the orphaned message as failed so
        // for_stack() excludes it from the context we send to the LLM.
        let prior = chat_history::for_stack(pool, stack.id).await?;
        if let Some(last) = prior.last() {
            if matches!(last.role, chat_history::Role::User | chat_history::Role::Agent) {
                warn!(session_id = self.session_id, message_id = last.id, "orphaned user message (cancelled turn) — marking failed");
                chat_history::mark_failed(pool, last.id).await?;
            }
        }

        let user_content    = content.to_string(); // save before TurnOutcome::Final shadows `content`
        let user_message_id = chat_history::append_with_metadata(pool, stack.id, &chat_history::Role::User, content, is_synthetic, None, metadata.as_ref()).await?;

        // Telnet-style echo: the bubble appears only once the message is persisted.
        // Synthetic turns (TIC/notification) never produce a user bubble.
        if !is_synthetic {
            let attachments = metadata.as_ref().map(|m| m.attachments.clone()).unwrap_or_default();
            // A custom slash command persists its expanded template (for LLM replay)
            // but the bubble must show the typed command — emit `display` when present.
            let echo = metadata.as_ref()
                .and_then(|m| m.command.as_ref())
                .map(|c| c.display.clone())
                .unwrap_or_else(|| user_content.clone());
            em.user_message(user_message_id, echo, attachments).await;
        }

        // Resume any tool calls left pending from a previous interrupted session.
        // They are re-gated (rules may have changed) and executed before the LLM runs.
        self.resume_pending_tools(stack.id, &config, &token, &tx).await?;

        let outcome = self.run_agent_turn(stack.id, &config, &token, &tx, pending_input.as_ref()).await?;

        match outcome {
            TurnOutcome::Final { content, message_id, input_tokens, output_tokens, truncated, tool_calls } => {
                // Persist token count so the *next* handle_message call knows
                // whether to compact before running the LLM loop.
                if let Some(t) = input_tokens {
                    self.last_input_tokens.store(t, Ordering::Relaxed);
                }
                info!(session_id = self.session_id, stack_id = stack.id, ?input_tokens, ?output_tokens, "handle_message done");
                if truncated {
                    warn!(session_id = self.session_id, ?output_tokens, "response truncated (max_tokens)");
                    em.truncated(output_tokens).await;
                }
                em.done(message_id, stack.id, content.clone(), input_tokens, output_tokens).await;

                // Publish both messages to the event bus now that both are in the DB.
                let now = chrono::Utc::now();
                self.event_bus.user_message(ChatEvent {
                    session_id:     self.session_id,
                    stack_id:       stack.id,
                    message_id:     user_message_id,
                    role:           ChatEventRole::User,
                    content:        user_content,
                    is_synthetic,
                    is_interactive: self.is_interactive,
                    is_ephemeral:   self.is_ephemeral,
                    tool_calls:     vec![],
                    created_at:     now,
                });
                self.event_bus.assistant_response(ChatEvent {
                    session_id:     self.session_id,
                    stack_id:       stack.id,
                    message_id,
                    role:           ChatEventRole::Assistant,
                    content,
                    is_synthetic:   false,
                    is_interactive: self.is_interactive,
                    is_ephemeral:   self.is_ephemeral,
                    tool_calls,
                    created_at:     now,
                });

                Ok(())
            }
            TurnOutcome::Cancelled => {
                info!(session_id = self.session_id, "handle_message cancelled by user");
                em.error("Cancelled by user.".to_string()).await;
                Err(anyhow::anyhow!("Turn cancelled by user"))
            }
            TurnOutcome::Exhausted => {
                error!(session_id = self.session_id, max_rounds = self.max_tool_rounds, "tool-call loop exhausted without final answer");
                em.error(format!("Exceeded {} tool-call rounds without a final answer.", self.max_tool_rounds)).await;
                Err(anyhow::anyhow!("tool-call loop exhausted after {} rounds without a final answer", self.max_tool_rounds))
            }
        }
    }
}
