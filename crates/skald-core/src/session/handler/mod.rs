use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use async_trait::async_trait;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, mpsc};

use tracing::{error, info, trace, warn};

use crate::approval::ApprovalManager;
use crate::run_context::RunContext;
use crate::tools::tool_names as tn;
use crate::chat_event_bus::{ChatEvent, ChatEventBus, ChatEventRole};
use crate::clarification::ClarificationManager;
use crate::compactor::ContextCompactor;
use crate::db::{chat_history, chat_sessions_stack};
use crate::events::ServerEvent;
use core_api::message_meta::MessageMetadata;
use core_api::user_fs::{SharedFs, UserFs};
use crate::llm::LlmManager;
use crate::mcp::McpProvider;
use crate::image_generate::ImageGeneratorManager;
use crate::memory::MemoryManager;
use crate::tools::ToolRegistry;

pub(crate) mod config;
mod kernel_turn;
pub(crate) mod interface_tools;
pub mod media;


pub use interface_tools::{InterfaceTool, ToolFuture};

pub const DEFAULT_MAX_TOOL_ROUNDS: usize = 20;

/// Default maximum number of synchronous sub-agents dispatched concurrently when
/// the LLM emits a homogeneous batch of sub-agent calls in a single response.
/// Bounds fan-out so a large batch does not trigger provider rate-limit storms.
pub const DEFAULT_MAX_PARALLEL_SUBAGENTS: usize = 4;

pub(crate) const MAX_AGENT_DEPTH: i64 = 5;

/// A queued user message to be appended to history mid-turn (drained from the
/// source inbox at a round boundary).
pub struct PendingMsg {
    pub content:  String,
    pub metadata: Option<MessageMetadata>,
}

/// Source of queued user input for the in-flight turn. Implemented by `ChatHub`
/// over a source's inbox; it lets the kernel pull newly-queued user
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

/// What a turn ended as, for the caller of `handle_message`. Deliberately
/// thinner than the kernel's outcome: the content the UI shows (`Done`,
/// `Truncated`, the reasoning trace) is already on the wire by the time a turn
/// returns — the event translator emitted it live — so what is left here is
/// what the app still has to do afterwards (publish on the chat bus, record
/// token counts for the compaction threshold).
pub(super) enum TurnOutcome {
    Final {
        content:       String,
        message_id:    i64,
        input_tokens:  Option<u32>,
        output_tokens: Option<u32>,
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
pub(crate) fn preview_truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => format!("{}…", &s[..byte_idx]),
        None                => s.to_string(),
    }
}

pub(crate) fn update_scratchpad_tool_def() -> Value {
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
pub(crate) fn write_todos_tool_def() -> Value {
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
/// synchronous sub-agent. The behaviour is the crate's `DelegateTool`; this is
/// the legacy schema it is advertised with, kept byte-for-byte (D11).
pub(crate) fn execute_subtask_tool_def() -> Value {
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

pub(crate) fn ask_user_clarification_tool_def() -> Value {
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
    /// Round budget, for the error message when a turn exhausts it. Every other
    /// loop limit (history window, result caps, fan-out width, datetime block)
    /// belongs to the turn, so it lives on the `UserLoopRuntime`'s `LoopConfig`.
    pub(super) max_tool_rounds:  usize,
    pub(super) agent_id:         String,
    /// Source of the session: "web", "telegram", "cron", etc.
    pub(super) source:           String,
    /// True when a real user is actively participating (web, telegram).
    pub(super) is_interactive:   bool,
    /// True for short-lived automated sessions (cron, tic).
    pub(super) is_ephemeral:     bool,
    pub(super) tools:            Arc<ToolRegistry>,
    pub(super) mcp:              Arc<dyn McpProvider>,
    pub(super) approval:         Arc<ApprovalManager>,
    pub(super) clarification:    Arc<ClarificationManager>,
    pub(super) event_bus:        Arc<ChatEventBus>,
    /// Human-readable label injected by background runners (e.g. "CronJob: Daily Digest").
    pub(super) context_label:    Arc<std::sync::RwLock<Option<String>>>,
    pub(super) memory_manager:         Arc<MemoryManager>,
    pub(super) image_generator_manager: Arc<ImageGeneratorManager>,
    /// Prevents concurrent handle_message calls on the same session.
    pub(super) processing:       Mutex<()>,
    /// When true, any tool call that would require human approval is automatically
    /// denied instead of blocking. Used by TicManager and other headless runners
    /// that cannot process approval requests.
    pub(super) auto_deny_approvals: Arc<AtomicBool>,
    /// Tool-call ids the user already approved via a resolve endpoint after a restart
    /// (no live oneshot to unblock). The next resume's approval gate skips re-gating
    /// these so a post-restart approve dispatches the tool without a second prompt.
    pub(super) pre_approved:     Arc<std::sync::Mutex<std::collections::HashSet<i64>>>,
    /// Context compactor, shared across all sessions.  `None` when compaction
    /// is disabled (no `compaction` section in config).
    pub(super) compactor:        Option<Arc<ContextCompactor>>,
    /// This user's loop stack (manager, store, gate, catalog, delegate), built
    /// once per `ChatSessionManager` and shared by every session of the owner.
    pub(super) loop_runtime:     Arc<crate::loop_adapters::runtime::UserLoopRuntime>,
    /// Input token count from the most recently completed turn, stored
    /// atomically so the next `handle_message` call can decide whether to
    /// compact before processing the new message.  Zero means unknown
    /// (provider did not report usage on the first turn).
    pub(super) last_input_tokens: AtomicU32,
    /// Active RunContext for this session. `None` means the "default" group is used implicitly.
    pub(super) run_context: Arc<tokio::sync::RwLock<Option<RunContext>>>,
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
        max_tool_rounds:       usize,
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
        loop_runtime:             Arc<crate::loop_adapters::runtime::UserLoopRuntime>,
    ) -> Self {
        Self {
            session_id,
            db,
            shared_pool,
            user_id,
            fs,
            llm_manager,
            max_tool_rounds,
            agent_id,
            source,
            is_interactive,
            is_ephemeral,
            tools,
            mcp,
            approval,
            clarification,
            event_bus,
            memory_manager,
            image_generator_manager,
            compactor,
            context_label:          Arc::new(std::sync::RwLock::new(None)),
            processing:             Mutex::new(()),
            auto_deny_approvals:    Arc::new(AtomicBool::new(false)),
            pre_approved:           Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            last_input_tokens:      AtomicU32::new(0),
            run_context:            Arc::new(tokio::sync::RwLock::new(run_context)),
            scratchpad_session_id:  std::sync::OnceLock::new(),
            loop_runtime,
        }
    }

    /// Sets the human-readable context label for this session (e.g. "CronJob: Daily Digest").
    /// Called by background runners after the handler is created.
    pub fn set_context_label(&self, label: impl Into<String>) {
        if let Ok(mut g) = self.context_label.write() {
            *g = Some(label.into());
        }
    }

    /// The caller's current filesystem snapshot (home + shared folders + projects
    /// + docs). Cheap — clones an `Arc`. Used by upload persistence to place files
    /// in the owner's home.
    pub fn user_fs(&self) -> Arc<UserFs> {
        self.fs.load()
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

    /// Cancels the in-flight turn. The manager cancels the conversation's live
    /// loop, and every frame under it holds a child of that token — so a `/stop`
    /// is sticky across sub-agent recursion, and lands on the next round
    /// boundary, on the in-flight LLM call, and on cancellable tools
    /// (e.g. `execute_cmd`).
    pub fn cancel(&self) {
        self.cancel_kernel_turn();
    }

    /// True if a turn is currently in flight (the `processing` mutex is held for
    /// the whole duration of `handle_message` / a recovery). Used to tell a
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
    /// leaving the tool as 'pending' so the next recovery re-asks on reconnect.
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
                compactor.force_compact(
                    self.loop_runtime.manager(), pool, self.session_id, stack.id, self.is_ephemeral,
                ).await
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
        // The projection derives the LLM-facing block; the UI renders chips.
        metadata:                     Option<MessageMetadata>,
        // Queued user input for this source. When `Some`, the kernel drains
        // it at each round boundary and injects newly-arrived user messages into
        // the running turn. `None` for sub-agents / resume / non-interactive runners.
        pending_input:                Option<Arc<dyn PendingUserInput>>,
    ) -> anyhow::Result<()> {
        let _guard = self.processing.lock().await;
        // NB: the turn's cancellation scope is the manager's — minted by
        // `start_turn` and cloned by value down the whole call tree, so a /stop
        // is sticky across sub-agent recursion (see `cancel`).
        let pool   = &self.db;
        let user_content = content.to_string(); // saved for the ChatEvent publication

        // Retrieve memory context (Honcho or other backend) for this turn.
        // Kept SEPARATE from extra_system_context (the static part) so it can be
        // injected as a dynamic tail system message after the conversation history
        // rather than embedded in the cacheable static prefix.  This allows
        // providers with prefix caching (e.g. Alibaba/DeepSeek via OpenRouter)
        // to cache the stable system prompt across turns even though Honcho
        // memories change on every call.
        let honcho_dynamic = match self.memory_manager.query_context(&self.user_id, self.session_id, content).await {
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
                // Lazy root frame: run this session's own entry agent (the same id
                // `build_agent_config` resolves the prompt from), never a hardcoded default.
                chat_sessions_stack::create(pool, self.session_id, &self.agent_id, None, 0, None).await?
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
            match compactor.try_compact(
                self.loop_runtime.manager(), pool, self.session_id, stack.id, last_tokens, self.is_ephemeral,
            ).await {
                Ok(true)  => info!(session_id = self.session_id, stack_id = stack.id, "handle_message: context compacted"),
                Ok(false) => {}
                Err(e)    => warn!(session_id = self.session_id, error = %e, "handle_message: compaction failed (non-fatal), continuing"),
            }
        }
        // ─────────────────────────────────────────────────────────────────────

        // NB: a trailing orphan User/Agent message (a turn cancelled before the
        // LLM answered, which breaks the alternation strict APIs require) is
        // marked failed by `LoopManager::start_turn` — it is a well-formedness
        // rule of the history, so the library owns it, and it runs there at the
        // right moment: right before the new user message is appended.

        // NB: tool calls left dangling by an interrupted session are repaired
        // inside `run_kernel_turn` — it owns the event translator, so the
        // re-execution's cards reach the client like any other.

        let outcome = self.run_kernel_turn(
            &config, content, is_synthetic, metadata.as_ref(), pending_input.as_ref(), &tx,
        ).await?;

        match outcome {
            TurnOutcome::Final { content, message_id, input_tokens, output_tokens, tool_calls } => {
                // Persist token count so the *next* handle_message call knows
                // whether to compact before running the LLM loop.
                if let Some(t) = input_tokens {
                    self.last_input_tokens.store(t, Ordering::Relaxed);
                }
                info!(session_id = self.session_id, stack_id = stack.id, ?input_tokens, ?output_tokens, "handle_message done");
                // NB: the WS echo (UserMessage), the Done and — when cut off —
                // the Truncated events were already emitted by the kernel's
                // event translator during the turn.

                // Publish both messages to the event bus now that both are in the DB.
                let user_message_id = shared_user_message_id(&self.db, stack.id, message_id).await;
                let now = chrono::Utc::now();
                self.event_bus.user_message(ChatEvent {
                    session_id:     self.session_id,
                    stack_id:       stack.id,
                    user_id:        self.user_id.clone(),
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
                    user_id:        self.user_id.clone(),
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
                // The "Cancelled by user." error event was already emitted by
                // the translator (root LoopEvent::Cancelled).
                Err(anyhow::anyhow!("Turn cancelled by user"))
            }
            TurnOutcome::Exhausted => {
                error!(session_id = self.session_id, max_rounds = self.max_tool_rounds, "tool-call loop exhausted without final answer");
                tx.send(ServerEvent::Error {
                    message: format!("Exceeded {} tool-call rounds without a final answer.", self.max_tool_rounds),
                }).await.ok();
                Err(anyhow::anyhow!("tool-call loop exhausted after {} rounds without a final answer", self.max_tool_rounds))
            }
        }
    }
}

/// The user message of the current turn: the latest User row before the final
/// assistant message (used for the ChatEvent publication).
async fn shared_user_message_id(pool: &sqlx::SqlitePool, stack_id: i64, _final_id: i64) -> i64 {
    let history = chat_history::for_stack(pool, stack_id).await.unwrap_or_default();
    history
        .iter()
        .rev()
        .find(|m| matches!(m.role, chat_history::Role::User | chat_history::Role::Agent))
        .map(|m| m.id)
        .unwrap_or_default()
}
