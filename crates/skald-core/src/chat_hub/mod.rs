use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use core_api::message_meta::Attachment;
use sqlx::SqlitePool;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

mod inbox;
use inbox::{QueuedMessage, SourceInbox, build_unit, drain_leading_user};

use crate::approval::ApprovalManager;
use crate::cron::TaskManager;
use crate::db::{chat_history, chat_llm_tools, chat_sessions, chat_sessions_stack, config, sources};
use crate::events::{GlobalEvent, ServerEvent};
use crate::notification::Notification;
use crate::session::handler::{ChatSessionHandler, InterfaceTool, PendingMsg, PendingUserInput};
use crate::session::manager::ChatSessionManager;
use crate::tools::tool_names as tn;

pub use core_api::chat_hub::{ChatHubApi, ModelCommandOutcome, SendMessageOptions};

pub const HOME_SOURCE_KEY:     &str = "source_home";
pub const DEFAULT_HOME_SOURCE: &str = "web";

// Global broadcast channel capacity.
const EVENTS_CAPACITY: usize = 512;

// Central notification queue capacity (inbound from background agents).
const NOTIFY_CAPACITY: usize = 64;

// How long to wait after the first notification before draining, to batch bursts.
const NOTIFY_BATCH_WINDOW_MS: u64 = 200;

// Idle-debounce for per-source message coalescing. 0 = pure coalesce-while-busy
// (a message to an idle source dispatches immediately). Raise it to also batch
// messages sent rapidly to an idle source, at the cost of that latency on the
// first message of a burst.
const SOURCE_COALESCE_DEBOUNCE_MS: u64 = 0;

// ── ChatHub ───────────────────────────────────────────────────────────────────

/// Manages **interactive, user-facing sessions only** (web, mobile, project chats):
/// one live, persistent session per `source`, reachable over WebSocket and addressed
/// by source id through the `sources` table.
///
/// It is **not** a runner for background / non-interactive agents (cron jobs, TIC,
/// sub-agent tasks). Those go through `TaskManager` / `ChatSessionManager` directly and
/// must not be routed here — they are not user-facing, have no broadcast audience, and
/// should not appear in the `sources` table. (Historically this class was misused to
/// drive non-interactive agents; keep that boundary.)
pub struct ChatHub {
    db:          Arc<SqlitePool>,
    session_mgr: Arc<ChatSessionManager>,
    pub approval: Arc<ApprovalManager>,
    /// Single global broadcast bus. All events from all sources flow here,
    /// wrapped in GlobalEvent with source/session_id tags. Subscribers filter.
    global_tx:   broadcast::Sender<GlobalEvent>,
    /// Central inbound notification queue from background agents.
    /// Consumer task is spawned in new() and drains this channel.
    notify_tx:   mpsc::Sender<Notification>,
    /// TaskManager reference for injecting execute_task into interactive sessions.
    /// Set via set_task_mgr() after construction (breaks circular dep with cron).
    task_mgr:    std::sync::OnceLock<Arc<TaskManager>>,
    /// Per-source input inboxes (coalescing + FIFO ordering). Created lazily on the
    /// first message for a source; each spawns one consumer task.
    inboxes:     Mutex<HashMap<String, Arc<SourceInbox>>>,
    /// Weak self-reference, set in `new()`, so lazily-spawned source consumers can
    /// reach back into the hub to dispatch turns.
    me:          OnceLock<Weak<Self>>,
    /// Shutdown token, used to stop lazily-spawned source consumers.
    shutdown:    CancellationToken,
    /// Per-source pinned LLM client (e.g. set via `/model` or the web dropdown).
    /// Keyed by source id; value is a `client_names()` entry (`"auto"` or a
    /// model name). When absent the caller AUTO-resolves. In-memory only: a
    /// server restart clears all pins (intentional for the MVP).
    selected_clients: Mutex<HashMap<String, String>>,
    /// The entry agent used when a source has no session yet and the caller did
    /// not specify one. Resolved once, at login, from the owner's role
    /// (`attrs.chat_agent`, else `DEFAULT_CHAT_AGENT`) — this hub is owner-bound,
    /// so its default is the owner's default. Every lazy `get_or_create_session`
    /// path (WS connect, notify, synthetic turns) routes through it, so a member's
    /// role-assigned assistant is honored regardless of which path creates the
    /// first session.
    default_agent: String,
}

impl ChatHub {
    pub fn new(
        db:          Arc<SqlitePool>,
        session_mgr: Arc<ChatSessionManager>,
        approval:    Arc<ApprovalManager>,
        global_tx:   broadcast::Sender<GlobalEvent>,
        shutdown:    CancellationToken,
        default_agent: String,
    ) -> Arc<Self> {
        let (notify_tx, notify_rx) = mpsc::channel::<Notification>(NOTIFY_CAPACITY);

        let hub = Arc::new(Self {
            db,
            session_mgr,
            approval,
            global_tx,
            notify_tx,
            task_mgr: std::sync::OnceLock::new(),
            inboxes:  Mutex::new(HashMap::new()),
            me:       OnceLock::new(),
            shutdown: shutdown.clone(),
            selected_clients: Mutex::new(HashMap::new()),
            default_agent,
        });
        // Store a weak self-reference for lazily-spawned source consumers.
        let _ = hub.me.set(Arc::downgrade(&hub));

        // Spawn the background consumer with a Weak reference so it doesn't
        // prevent ChatHub from being dropped on shutdown.
        tokio::spawn(Self::notification_consumer(Arc::downgrade(&hub), notify_rx, shutdown));

        hub
    }

    /// Called once after TaskManager is built (breaks circular dep: TaskManager needs
    /// ChatSessionManager, ChatHub needs TaskManager for execute_task injection).
    pub fn set_task_mgr(&self, task_mgr: Arc<TaskManager>) {
        let _ = self.task_mgr.set(task_mgr);
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Register a source. No-op for duplicate registrations.
    /// With the global bus, registration no longer creates a per-source channel.
    pub async fn register(&self, source_id: &str) {
        info!(source_id, "ChatHub: source registered");
    }

    /// Enqueue a user message for a source. Returns immediately once queued; the
    /// turn runs asynchronously on the source's consumer task, which coalesces
    /// messages that pile up during an in-flight turn into a single follow-up turn
    /// (see `inbox`). Creates the source's inbox (and consumer) lazily on first use.
    /// Turn errors surface via the `Error` event on the broadcast bus, not this
    /// return value.
    pub async fn send_message(
        &self,
        source_id: &str,
        prompt:    &str,
        opts:      SendMessageOptions,
    ) -> anyhow::Result<()> {
        let inbox = self.get_or_spawn_inbox(source_id).await;
        inbox.pending.lock().await.push_back(QueuedMessage {
            prompt: prompt.to_string(),
            opts,
        });
        inbox.notify.notify_one();
        Ok(())
    }

    /// Returns the source's inbox, creating it (and spawning its consumer) on first use.
    async fn get_or_spawn_inbox(&self, source_id: &str) -> Arc<SourceInbox> {
        let mut inboxes = self.inboxes.lock().await;
        if let Some(inbox) = inboxes.get(source_id) {
            return Arc::clone(inbox);
        }
        let inbox = Arc::new(SourceInbox::default());
        inboxes.insert(source_id.to_string(), Arc::clone(&inbox));
        let weak = self.me.get().expect("ChatHub::me must be set in new()").clone();
        tokio::spawn(Self::source_consumer(
            weak,
            source_id.to_string(),
            Arc::clone(&inbox),
            self.shutdown.clone(),
        ));
        info!(source_id, "ChatHub: source inbox + consumer spawned");
        inbox
    }

    /// Runs one LLM turn for a coalesced unit: resolves session/handler, bridges
    /// events to the global bus, injects `execute_task`, and calls `handle_message`
    /// (which takes the per-session `processing` lock).
    async fn dispatch_turn(
        &self,
        source_id: &str,
        prompt:    &str,
        opts:      SendMessageOptions,
        // Live user-input source for this turn (the source's inbox). The running
        // turn drains it at each round boundary to inject messages queued while it
        // was busy. `None` for synthetic turns, which never inject.
        pending_input: Option<Arc<dyn PendingUserInput>>,
    ) -> anyhow::Result<()> {
        let agent_id = opts.agent_id.as_deref().unwrap_or(&self.default_agent);
        let session_id = self.get_or_create_session(source_id, agent_id).await?;
        let source_tag = source_id.to_string();

        // Bridge mpsc from handle_message → global broadcast, tagging with source/session.
        let tx = Self::bridge_to_global(self.global_tx.clone(), source_tag, session_id);

        // get_or_create_handler is idempotent; we call it early to read the
        // session's RunContext so it can be inherited by any task spawned here.
        let handler = self.session_mgr.get_or_create_handler(session_id).await?;
        let run_context_json = handler.run_context_json().await;

        // Inject execute_task as an InterfaceTool for all interactive sessions.
        // session_id and run_context_json are captured so tasks inherit the parent context.
        let mut interface_tools = opts.interface_tools;
        if let Some(task_mgr) = self.task_mgr.get() {
            interface_tools.push(
                crate::tools::cron_jobs::build_execute_task_interface_tool(
                    Arc::clone(task_mgr),
                    session_id,
                    run_context_json,
                )
            );
        }
        handler.handle_message(
            prompt,
            opts.client_name,
            opts.extra_system_context,
            opts.extra_system_dynamic,
            opts.tail_reminder,
            interface_tools,
            opts.system_substitutions,
            tx,
            opts.is_synthetic,
            opts.metadata,
            pending_input,
        ).await
    }

    /// Returns the session handler for the source's active session, creating one lazily if needed.
    pub async fn session_handler(&self, source_id: &str) -> anyhow::Result<Arc<ChatSessionHandler>> {
        let session_id = self.get_or_create_session(source_id, &self.default_agent).await?;
        self.session_mgr.get_or_create_handler(session_id).await
    }

    /// Persist an uploaded file for `source_id` into the owner's home
    /// (`~/uploads/{session}/`) and return its [`Attachment`]. The single entry
    /// point every surface (web handler, channel plugins) routes through, so
    /// uploads can't drift on placement or on the recorded agent path — see
    /// [`crate::uploads::save_to_home`]. Resolves the source's active session (so
    /// the upload shares the directory the following message references).
    pub async fn save_upload(
        &self,
        source_id: &str,
        file_name: &str,
        client_mime: Option<String>,
        bytes: &[u8],
    ) -> anyhow::Result<Attachment> {
        let handler = self.session_handler(source_id).await?;
        let fs = handler.user_fs();
        let att = crate::uploads::save_to_home(
            &fs,
            handler.session_id,
            file_name,
            client_mime,
            bytes,
        )
        .await?;
        Ok(att)
    }

    /// Returns the handler for a specific `session_id`, creating one lazily if needed.
    /// Used to resolve a pending tool against the session that actually owns it,
    /// independent of any source's "active" session.
    pub async fn handler_for_session(&self, session_id: i64) -> anyhow::Result<Arc<ChatSessionHandler>> {
        self.session_mgr.get_or_create_handler(session_id).await
    }

    /// Ensures a persistent, interactive session exists for `source`, created with
    /// `agent_id` and the given `run_context`.
    ///
    /// If a session already exists for the source it is returned as-is, unless `reset`
    /// is set — in which case the existing session is discarded and a fresh one is
    /// created (and a `NewSession` event is broadcast so connected clients reset).
    ///
    /// This is the single entry point for the source→session mapping ChatHub owns.
    /// Note: `agent_id`/`run_context` only take effect when a session is actually
    /// created; on reuse the existing session keeps its original agent and context.
    pub async fn provision_session(
        &self,
        source_id:   &str,
        agent_id:    &str,
        run_context: Option<&crate::run_context::RunContext>,
        reset:       bool,
    ) -> anyhow::Result<i64> {
        // A reset discards the current session; drop any messages queued for it.
        if reset {
            self.clear_inbox(source_id).await;
        }
        if !reset {
            if let Some(sid) = sources::active_session_id(&self.db, source_id).await? {
                return Ok(sid);
            }
        }
        let (session_id, _) = self.session_mgr
            .create_session(agent_id, source_id, true, false, run_context)
            .await?;
        sources::upsert(&self.db, source_id, session_id).await?;
        info!(source_id, session_id, agent_id, reset, "ChatHub: session provisioned");
        if reset {
            let _ = self.global_tx.send(GlobalEvent {
                source:     Some(source_id.to_string()),
                session_id: Some(session_id),
                event:      ServerEvent::NewSession { session_id },
            });
        }
        Ok(session_id)
    }

    /// Create a new session for the source, discarding the previous one.
    /// Thin wrapper over `provision_session` using the owner's default entry agent
    /// (kept for the `ChatHubApi` trait and generic callers).
    pub async fn clear(&self, source_id: &str) -> anyhow::Result<i64> {
        self.provision_session(source_id, &self.default_agent, None, true).await
    }

    /// Subscribe to the global event bus. The `source_id` parameter is accepted
    /// for API compatibility but filtering by source is the caller's responsibility.
    pub fn events(&self, _source_id: &str) -> broadcast::Receiver<GlobalEvent> {
        self.global_tx.subscribe()
    }

    /// Emit an event directly on the global bus (for system events without a session).
    pub fn emit(&self, event: GlobalEvent) {
        let _ = self.global_tx.send(event);
    }

    /// Set which source is the "home" for background agent notifications.
    pub async fn set_home(&self, source_id: &str) -> anyhow::Result<()> {
        config::set(&self.db, HOME_SOURCE_KEY, source_id).await?;
        info!(source_id, "ChatHub: home source set");
        Ok(())
    }

    /// Returns the current home source id, falling back to `web` if not configured.
    pub async fn home_source(&self) -> anyhow::Result<String> {
        Ok(config::get(&self.db, HOME_SOURCE_KEY)
            .await?
            .unwrap_or_else(|| DEFAULT_HOME_SOURCE.to_string()))
    }

    /// Returns token usage for the last message in the source's active session.
    /// Returns `(input_tokens, output_tokens)` — both are `None` when no
    /// messages exist or the provider did not report usage.
    pub async fn context_info(&self, source_id: &str) -> anyhow::Result<(Option<i64>, Option<i64>)> {
        let session_id = self.get_or_create_session(source_id, &self.default_agent).await?;
        let stack = match chat_sessions_stack::active_for_session(&self.db, session_id).await? {
            Some(s) => s,
            None => return Ok((None, None)),
        };
        let last = chat_history::last_message_for_stack(&self.db, stack.id).await?;
        Ok(last.map_or((None, None), |m| (m.input_tokens, m.output_tokens)))
    }

    /// Total spend (USD) of the source's active session, including synchronous
    /// sub-agent frames and excluding asynchronous tasks (which run in their own
    /// session). `None` when no provider reported a cost.
    pub async fn cost_info(&self, source_id: &str) -> anyhow::Result<Option<f64>> {
        let session_id = self.get_or_create_session(source_id, &self.default_agent).await?;
        chat_history::total_cost_for_session(&self.db, session_id).await
    }

    /// Force compaction of the source's active session history.
    /// Bypasses the token threshold; returns `true` if compaction occurred.
    pub async fn force_compact(&self, source_id: &str) -> anyhow::Result<bool> {
        let handler = self.session_handler(source_id).await?;
        handler.force_compact().await
    }

    /// Resume any interrupted turn for a source's active session.
    /// Calls `resume_turn` which re-executes pending tool calls (approval or
    /// clarification) and re-runs the LLM loop if needed.
    /// Safe to call unconditionally — returns immediately if there is nothing to resume.
    /// Events are published to the global broadcast bus so existing subscribers
    /// (e.g. Telegram's persistent_forwarder) receive them without a WS connection.
    pub async fn resume(&self, source_id: &str) -> anyhow::Result<()> {
        let session_id = match sources::active_session_id(&self.db, source_id).await? {
            Some(sid) => sid,
            None      => return Ok(()), // no prior session, nothing to resume
        };
        // Guard against double-driving. A client sends `resume` on connect whenever
        // history shows a pending/interrupted tool — including when the turn is still
        // live and merely awaiting an approval. Without this check `resume_turn` would
        // block on the `processing` lock and, once the approval unblocks the original
        // turn and it finishes, run a spurious *second* turn on the just-completed
        // conversation. If a turn is already in flight it owns the session and emits
        // its own events, so there is nothing to resume — skip.
        if let Ok(handler) = self.session_handler(source_id).await {
            if handler.is_processing() {
                info!(source_id, "ChatHub::resume: turn already in flight — skipping resume");
                return Ok(());
            }
        }
        self.resume_session(session_id).await
    }

    /// Resume an interrupted turn for a specific `session_id` (post-restart recovery
    /// or after a manual approval resolve), independent of any source's active session.
    /// Injects `execute_task` so a pending sub-agent task can be re-dispatched, and
    /// bridges events to the global bus so the reconnected client still sees them.
    pub async fn resume_session(&self, session_id: i64) -> anyhow::Result<()> {
        // Source tag drives per-source event filtering for connected clients.
        let source = chat_sessions::find_by_id(&self.db, session_id).await?
            .map(|s| s.source)
            .unwrap_or_else(|| "web".to_string());
        let tx = Self::bridge_to_global(self.global_tx.clone(), source, session_id);
        let handler = self.session_mgr.get_or_create_handler(session_id).await?;
        let interface_tools = self.execute_task_tools(session_id, &handler).await;
        handler.resume_turn(None, None, interface_tools, tx).await
    }

    /// Builds the `execute_task` interface tool for a session, mirroring the injection
    /// done for live turns (`run_agent_turn`). Empty when no TaskManager is configured
    /// so `execute_task mode=async` can be rebuilt by `build_execution` during resume.
    async fn execute_task_tools(
        &self,
        session_id: i64,
        handler:    &Arc<ChatSessionHandler>,
    ) -> Vec<InterfaceTool> {
        let mut tools = Vec::new();
        if let Some(task_mgr) = self.task_mgr.get() {
            let run_context_json = handler.run_context_json().await;
            tools.push(crate::tools::cron_jobs::build_execute_task_interface_tool(
                Arc::clone(task_mgr),
                session_id,
                run_context_json,
            ));
        }
        tools
    }

    /// Queue a structured notification from a background agent.
    /// The consumer task aggregates pending notifications and dispatches them to the home source.
    pub async fn notify(&self, note: Notification) -> anyhow::Result<()> {
        if self.notify_tx.send(note).await.is_err() {
            warn!("ChatHub::notify: notification queue full or receiver dropped");
        }
        Ok(())
    }

    /// Synchronous variant of `notify` for use inside `Tool::execute` (sync context).
    /// Uses `try_send` — drops the notification if the channel is full rather than blocking.
    pub fn notify_sync(&self, note: Notification) {
        if self.notify_tx.try_send(note).is_err() {
            warn!("ChatHub::notify_sync: notification channel full or closed — notification dropped");
        }
    }

    /// Revoke all session-scoped MCP grants for a source's active session.
    /// The next LLM turn will start with no MCP servers activated.
    pub async fn reset_mcp(&self, source_id: &str) -> anyhow::Result<()> {
        let session_id = self.get_or_create_session(source_id, &self.default_agent).await?;
        crate::db::session_mcp_grants::revoke_all(&self.db, session_id).await?;
        info!(source_id, session_id, "ChatHub: MCP grants reset");
        Ok(())
    }

    // ── Per-source pinned LLM client ─────────────────────────────────────────
    //
    // Backend-owned state: every UI mutation (Telegram `/model`, web `/model`,
    // web dropdown change) funnels through `set_selected_client`, which then
    // broadcasts `ClientSelected` to all clients of the source. The web dropdown
    // and mobile select read this event to stay in sync — the backend is the
    // single source of truth. Pattern is intentionally generic so future
    // per-source toggles (e.g. reasoning level) can mirror it.

    /// Returns `(models, default)` — `models` is the ordered list of usable
    /// client names (`"auto"` first, then models by priority/name), `default`
    /// is the configured default client name.
    pub async fn list_clients(&self) -> (Vec<String>, String) {
        let mgr = self.session_mgr.llm_manager();
        (mgr.client_names().await, mgr.default_name().await)
    }

    /// Returns the client name pinned for the source, or `None` when unset
    /// (the caller should fall back to AUTO resolution).
    pub async fn get_selected_client(&self, source_id: &str) -> Option<String> {
        self.selected_clients.lock().await.get(source_id).cloned()
    }

    /// Pin a client name for the source and broadcast `ClientSelected`.
    /// `client` should be a `list_clients()` entry (`"auto"` or a model name).
    pub async fn set_selected_client(&self, source_id: &str, client: String) {
        info!(source_id, client = %client, "ChatHub: selected client set");
        self.selected_clients.lock().await.insert(source_id.to_string(), client.clone());
        self.emit(GlobalEvent {
            source:     Some(source_id.to_string()),
            session_id: None,
            event:      ServerEvent::ClientSelected { client },
        });
    }

    /// Clear any pinned client for the source (revert to AUTO) and broadcast
    /// `ClientSelected { client: "auto" }`.
    pub async fn clear_selected_client(&self, source_id: &str) {
        info!(source_id, "ChatHub: selected client cleared (auto)");
        self.selected_clients.lock().await.remove(source_id);
        self.emit(GlobalEvent {
            source:     Some(source_id.to_string()),
            session_id: None,
            event:      ServerEvent::ClientSelected { client: "auto".to_string() },
        });
    }

    /// Snapshot of the model list with the per-source current selection marked.
    /// Returns `(index, name, is_current)` tuples so call sites can render
    /// HTML (Telegram) or Markdown (web) without re-querying the LLM manager
    /// or the pin store.
    pub async fn list_clients_marked(&self, source_id: &str) -> Vec<(usize, String, bool)> {
        let (models, _default) = self.list_clients().await;
        let current = self.get_selected_client(source_id).await
            .unwrap_or_else(|| "auto".to_string());
        models.into_iter()
            .enumerate()
            .map(|(i, name)| (i, name.clone(), name == current))
            .collect()
    }

    /// Apply a `/model {arg}` command: resolve the argument, mutate the
    /// per-source pinned client (broadcasting `ClientSelected`), return a
    /// structured outcome. Business logic is centralised here so Telegram and
    /// web share a single code path; only the formatting differs.
    pub async fn apply_model_command(
        &self,
        source_id: &str,
        arg: &str,
    ) -> ModelCommandOutcome {
        let (models, _default) = self.list_clients().await;
        match core_api::chat_hub::resolve_list_arg(&models, arg) {
            Ok(Some(client)) => {
                let name = client.clone();
                self.set_selected_client(source_id, client).await;
                ModelCommandOutcome::Set(name)
            }
            Ok(None) => {
                self.clear_selected_client(source_id).await;
                ModelCommandOutcome::Cleared
            }
            Err(msg) => ModelCommandOutcome::Error(msg),
        }
    }

    /// Cancel the active LLM turn for the source's session, clearing any pending
    /// approvals and clarification questions. No-op if no session is active.
    pub async fn cancel(&self, source_id: &str) {
        // Drop queued-but-not-yet-dispatched messages so /stop clears the backlog
        // too, not just the in-flight turn.
        self.clear_inbox(source_id).await;
        match self.session_handler(source_id).await {
            Ok(handler) => {
                handler.cancel();
                handler.cancel_pending_approvals().await;
                handler.cancel_pending_questions().await;
                info!(source_id, "ChatHub: cancel requested");
            }
            Err(e) => {
                warn!(source_id, error = %e, "ChatHub::cancel: no session to cancel");
            }
        }
    }

    /// Approve a pending tool-call approval request.
    pub async fn approve(&self, request_id: i64) {
        self.approval.approve(request_id).await;
    }

    /// Reject a pending tool-call approval request.
    pub async fn reject(&self, request_id: i64, note: String) {
        self.approval.reject(request_id, note).await;
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Spawn a bridge task that forwards events from an mpsc channel to the
    /// global broadcast bus, tagging each event with `source` and `session_id`.
    fn bridge_to_global(
        global_tx:  broadcast::Sender<GlobalEvent>,
        source:     String,
        session_id: i64,
    ) -> mpsc::Sender<ServerEvent> {
        let (tx, mut rx) = mpsc::channel::<ServerEvent>(EVENTS_CAPACITY);
        tokio::spawn(async move {
            tracing::debug!(%source, session_id, "ChatHub: bridge task started");
            while let Some(event) = rx.recv().await {
                tracing::debug!(%source, session_id, event_type = event.type_name(), "ChatHub: bridge forwarding event");
                let _ = global_tx.send(GlobalEvent {
                    source:     Some(source.clone()),
                    session_id: Some(session_id),
                    event,
                });
            }
            tracing::debug!(%source, session_id, "ChatHub: bridge task ended");
        });
        tx
    }

    async fn get_or_create_session(&self, source_id: &str, agent_id: &str) -> anyhow::Result<i64> {
        if let Some(sid) = sources::active_session_id(&self.db, source_id).await? {
            return Ok(sid);
        }
        let (session_id, _) = self.session_mgr.create_session(agent_id, source_id, true, false, None).await?;
        sources::upsert(&self.db, source_id, session_id).await?;
        info!(source_id, session_id, "ChatHub: session created lazily");
        Ok(session_id)
    }

    // ── Per-source inbox consumer ─────────────────────────────────────────────

    /// Per-source consumer: drains and coalesces queued messages, running one turn
    /// at a time. Spawned lazily by `get_or_spawn_inbox`; lives until shutdown.
    async fn source_consumer(
        hub:       Weak<Self>,
        source_id: String,
        inbox:     Arc<SourceInbox>,
        shutdown:  CancellationToken,
    ) {
        info!(%source_id, "ChatHub: source consumer started");
        loop {
            tokio::select! {
                _ = shutdown.cancelled()      => break,
                _ = inbox.notify.notified()   => {}
            }

            // Optional idle-batching window (0 = disabled).
            if SOURCE_COALESCE_DEBOUNCE_MS > 0 {
                tokio::time::sleep(Duration::from_millis(SOURCE_COALESCE_DEBOUNCE_MS)).await;
            }

            // Pop one message to seed a turn, then dispatch it. Messages that arrive
            // while the turn runs are injected live at its round boundaries (the turn
            // drains `pending` itself via the `PendingUserInput` handle below); only
            // messages that arrive after the turn's last boundary remain here and seed
            // the next turn on a following iteration.
            loop {
                let (unit, epoch) = {
                    let mut pending = inbox.pending.lock().await;
                    let epoch = inbox.cancel_epoch.load(Ordering::Acquire);
                    (build_unit(&mut pending), epoch)
                };
                let Some((prompt, opts)) = unit else { break };
                let Some(hub) = hub.upgrade() else { return };

                // A /stop between draining and dispatching bumps cancel_epoch and
                // clears pending — drop this now-stale unit.
                if inbox.cancel_epoch.load(Ordering::Acquire) != epoch {
                    continue;
                }

                // Live-injection source for this turn (real user turns only).
                let pending_input: Option<Arc<dyn PendingUserInput>> = (!opts.is_synthetic)
                    .then(|| Arc::new(InboxUserInput(Arc::clone(&inbox))) as Arc<dyn PendingUserInput>);

                // Run the turn on a dedicated task and await its handle. Isolating it
                // means a panic inside the turn (e.g. a UTF-8 boundary slice on a
                // tool-result preview) surfaces here as a JoinError and is logged,
                // instead of unwinding the consumer task and silently killing this
                // source's chat (new messages would then enqueue and never dispatch).
                let hub_turn = Arc::clone(&hub);
                let src = source_id.clone();
                let turn = tokio::spawn(async move {
                    hub_turn.dispatch_turn(&src, &prompt, opts, pending_input).await
                });
                match turn.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => error!(%source_id, error = %e, "ChatHub: source turn failed"),
                    Err(e)     => error!(%source_id, error = %e, "ChatHub: source turn panicked — consumer surviving"),
                }
            }
        }
        info!(%source_id, "ChatHub: source consumer stopped");
    }

    /// Clears a source's pending queue and bumps its cancel epoch (so a unit the
    /// consumer drained just before a `/stop` is dropped instead of dispatched).
    /// No-op if the source has no inbox yet.
    async fn clear_inbox(&self, source_id: &str) {
        if let Some(inbox) = self.inboxes.lock().await.get(source_id) {
            inbox.pending.lock().await.clear();
            inbox.cancel_epoch.fetch_add(1, Ordering::Release);
        }
    }

    // ── Notification consumer ─────────────────────────────────────────────────

    /// Background task: drains the central notification queue and dispatches
    /// aggregated briefings to the home source as synthetic user messages.
    ///
    /// Serialisation with active LLM turns is free: `ChatSessionHandler::handle_message`
    /// holds `processing: Mutex<()>` for the duration of a turn, so `send_message`
    /// below blocks naturally until the turn completes.
    async fn notification_consumer(hub: Weak<Self>, mut rx: mpsc::Receiver<Notification>, shutdown: CancellationToken) {
        info!("ChatHub: notification consumer started");

        loop {
            // Block until at least one notification arrives (or shutdown signal).
            let first = tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("ChatHub: notification consumer shutdown");
                    break;
                }
                msg = rx.recv() => match msg {
                    Some(n) => n,
                    None    => break, // notify_tx dropped — ChatHub is shutting down
                }
            };

            // Brief window to let burst notifications accumulate before dispatching.
            tokio::time::sleep(Duration::from_millis(NOTIFY_BATCH_WINDOW_MS)).await;

            // Drain everything else that arrived during the window.
            let mut notes = vec![first];
            while let Ok(n) = rx.try_recv() {
                notes.push(n);
            }

            let hub = match hub.upgrade() {
                Some(h) => h,
                None    => break, // ChatHub dropped
            };

            let home = match hub.home_source().await {
                Ok(h)  => h,
                Err(e) => { error!(error = %e, "notification consumer: home_source failed"); continue; }
            };

            let count = notes.len();
            // Build a synthetic assistant message with a reasoning trace and a
            // pre-completed read_notification tool call carrying the notifications as results.
            // The agent is then woken via resume() — resume_turn sees the tool calls on
            // the last assistant message and runs the LLM loop so the agent can respond.
            let result_json = serde_json::to_string(&notes).unwrap_or_else(|_| "[]".to_string());

            let session_id = match hub.get_or_create_session(&home, &hub.default_agent).await {
                Ok(sid) => sid,
                Err(e) => { error!(error = %e, "notification consumer: get_or_create_session failed"); continue; }
            };

            let stack = match chat_sessions_stack::active_for_session(&hub.db, session_id).await {
                Ok(Some(s)) => s,
                Ok(None)    => { error!(session_id, "notification consumer: no active stack"); continue; }
                Err(e)      => { error!(error = %e, "notification consumer: active_for_session failed"); continue; }
            };

            let assistant_id = match chat_history::append(
                &hub.db, stack.id, &chat_history::Role::Assistant,
                "", true,
                Some("The system signaled pending notifications. Let me read them and surface anything relevant to the user."),
            ).await {
                Ok(id) => id,
                Err(e) => { error!(error = %e, "notification consumer: append assistant failed"); continue; }
            };

            let tool_call_id = match chat_llm_tools::append(
                &hub.db, assistant_id, tn::READ_NOTIFICATION, "{}",
            ).await {
                Ok(id) => id,
                Err(e) => { error!(error = %e, "notification consumer: append tool call failed"); continue; }
            };

            if let Err(e) = chat_llm_tools::complete(&hub.db, tool_call_id, &result_json, "json").await {
                error!(error = %e, "notification consumer: complete tool call failed"); continue;
            }

            info!(home_source = %home, count, "ChatHub: dispatching notifications via read_notification");

            if let Err(e) = hub.resume(&home).await {
                error!(error = %e, "notification consumer: resume failed");
            }
        }

        info!("ChatHub: notification consumer stopped");
    }
}

// ── Live user-input source ──────────────────────────────────────────────────

/// Adapts a source's `SourceInbox` to the handler's `PendingUserInput` trait so a
/// running turn can drain newly-queued user messages at its round boundaries.
struct InboxUserInput(Arc<SourceInbox>);

#[async_trait]
impl PendingUserInput for InboxUserInput {
    async fn drain_user(&self) -> Vec<PendingMsg> {
        let mut pending = self.0.pending.lock().await;
        drain_leading_user(&mut pending)
            .into_iter()
            .map(|d| PendingMsg { content: d.content, metadata: d.metadata })
            .collect()
    }
}

// ── ChatHubApi impl ───────────────────────────────────────────────────────────

#[async_trait]
impl ChatHubApi for ChatHub {
    async fn register(&self, source_id: &str) {
        self.register(source_id).await
    }

    async fn send_message(
        &self,
        source_id: &str,
        prompt: &str,
        opts: SendMessageOptions,
    ) -> anyhow::Result<()> {
        self.send_message(source_id, prompt, opts).await
    }

    async fn save_upload(
        &self,
        source_id: &str,
        file_name: &str,
        client_mime: Option<String>,
        bytes: &[u8],
    ) -> anyhow::Result<Attachment> {
        self.save_upload(source_id, file_name, client_mime, bytes).await
    }

    async fn clear(&self, source_id: &str) -> anyhow::Result<i64> {
        self.clear(source_id).await
    }

    fn events(&self, source_id: &str) -> broadcast::Receiver<GlobalEvent> {
        self.events(source_id)
    }

    async fn set_home(&self, source_id: &str) -> anyhow::Result<()> {
        self.set_home(source_id).await
    }

    async fn context_info(&self, source_id: &str) -> anyhow::Result<(Option<i64>, Option<i64>)> {
        self.context_info(source_id).await
    }

    async fn cost_info(&self, source_id: &str) -> anyhow::Result<Option<f64>> {
        self.cost_info(source_id).await
    }

    async fn force_compact(&self, source_id: &str) -> anyhow::Result<bool> {
        self.force_compact(source_id).await
    }

    async fn resume(&self, source_id: &str) -> anyhow::Result<()> {
        self.resume(source_id).await
    }

    async fn approve(&self, request_id: i64) {
        self.approve(request_id).await
    }

    async fn reject(&self, request_id: i64, note: String) {
        self.reject(request_id, note).await
    }

    async fn resolve_question(&self, source_id: &str, request_id: i64, answer: String) {
        if let Ok(handler) = self.session_handler(source_id).await {
            handler.resolve_question(request_id, answer).await;
        } else {
            warn!(source_id, request_id, "ChatHubApi::resolve_question: no session handler");
        }
    }

    async fn cancel(&self, source_id: &str) {
        self.cancel(source_id).await
    }

    async fn reset_mcp(&self, source_id: &str) -> anyhow::Result<()> {
        self.reset_mcp(source_id).await
    }

    async fn list_clients(&self) -> (Vec<String>, String) {
        self.list_clients().await
    }

    async fn get_selected_client(&self, source_id: &str) -> Option<String> {
        self.get_selected_client(source_id).await
    }

    async fn set_selected_client(&self, source_id: &str, client: String) {
        self.set_selected_client(source_id, client).await;
    }

    async fn clear_selected_client(&self, source_id: &str) {
        self.clear_selected_client(source_id).await;
    }

    async fn list_clients_marked(
        &self,
        source_id: &str,
    ) -> Vec<(usize, String, bool)> {
        self.list_clients_marked(source_id).await
    }

    async fn apply_model_command(
        &self,
        source_id: &str,
        arg: &str,
    ) -> ModelCommandOutcome {
        self.apply_model_command(source_id, arg).await
    }
}
