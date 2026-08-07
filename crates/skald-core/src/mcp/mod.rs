use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use rand::RngExt;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::tools::ToolResult;

pub use mcp_client::{
    ElicitationHandler,
    McpCallResult, McpLogLine, McpLogTx, McpMedia, McpMediaData, McpMediaKind,
    McpServerClient, McpServerConfig, McpServerInfo, McpServerStatus, McpTool, McpTransport as McpTransportKind,
    parse_mcp_tool_name,
    http_server::McpHttpServer,
    server::{McpNotification, McpServer},
};

use mcp_client::McpTransport;

pub mod install;
mod logs;
pub mod oauth;
mod provider;
pub mod verify;

pub use install::{CONNECTORS_DIR, MANIFEST_FILE, connector_dir, ensure_installed_host, install_into_home, split_script_path};
pub use oauth::DeliverSpec;
pub use provider::{McpDirectoryHandle, McpProvider, SharedGlobalAccess, UserMcpView};
pub use verify::{VerifyReport, VerifyTarget, apply_placeholders, run_verify};

const SERVER_START_TIMEOUT_SECS: u64 = 120;

// ── Respawn policy ───────────────────────────────────────────────────────────
//
// A stdio connector *is* its child process, and that process can die under us —
// not only from a bug in the connector (the Gmail one segfaulted mid-call from a
// thread race on its HTTP connection) but from an OOM kill or a container blip.
// Nothing used to notice: the dead handle stayed in `servers`, every tool call on
// it answered "disconnected", and the connector's own background work (Gmail's
// new-mail polling) was silently over until the user next logged in.
//
// So a death is now reconciled, on the same terms as every other reconciliation
// in this codebase: best-effort, bounded, and settling at the next login if it
// fails. Two triggers share one seam (`restart_if_dead`) — a tool call, which
// repairs the connector in time for the call that noticed, and a periodic sweep,
// which is what gets a push-only connector back without anyone asking.

/// How often the sweep looks for a dead server. Sets the worst-case silence for a
/// connector whose only job is pushing notifications.
const RESPAWN_SWEEP_SECS: u64 = 10;

/// Consecutive restarts before the manager stops trying. A connector that dies
/// this many times in a row is broken in a way restarting does not fix, and the
/// spawn itself is not free (a container `docker exec`, an interpreter start).
const MAX_RESPAWN_ATTEMPTS: u32 = 5;

/// Base backoff between consecutive restarts; doubles per attempt up to
/// [`RESPAWN_BACKOFF_MAX_SECS`]. Enforced as a *time gate*, never as a sleep — a
/// tool call that finds the gate closed fails immediately rather than blocking a
/// user behind a crash-loop, and the sweep picks it up on a later tick.
const RESPAWN_BACKOFF_SECS: u64 = 2;
const RESPAWN_BACKOFF_MAX_SECS: u64 = 60;

/// Quiet period after which a server's attempt counter resets. Doubles as the
/// ceiling's escape hatch: a connector that exhausted its attempts is retried once
/// more after this long, so a box left running for weeks recovers from a transient
/// outage (a container recreated, a network gone and returned) instead of staying
/// dark until someone logs in again.
const RESPAWN_RESET_SECS: u64 = 300;

/// Per-server restart bookkeeping. Deliberately keyed on the last *attempt*, not on
/// uptime: what must be rate-limited is how often we spawn, and a server that dies
/// instantly on every try would otherwise never accumulate the uptime to be judged.
#[derive(Debug, Clone, Copy)]
struct RespawnState {
    attempts:     u32,
    last_attempt: std::time::Instant,
}

/// How long to wait after the n-th consecutive restart before trying again.
fn respawn_backoff(attempts: u32) -> Duration {
    let factor = 1u64.checked_shl(attempts.min(16)).unwrap_or(u64::MAX);
    Duration::from_secs(
        RESPAWN_BACKOFF_SECS.saturating_mul(factor).min(RESPAWN_BACKOFF_MAX_SECS)
    )
}

/// The restart policy, as a pure decision: given what happened last time, may we
/// spawn now — and if so, which attempt is this?
///
/// `None` refuses. `Some(n)` allows and reports the attempt number to record.
/// Split out from [`McpManager::claim_respawn`] because the policy is the subtle
/// part (notably: the reset window doubles as the ceiling's escape hatch, so
/// "gave up" is never permanent), while a manager needs a DB pool and a running
/// tokio runtime just to be constructed.
fn respawn_decision(prev: Option<RespawnState>, now: std::time::Instant) -> Option<u32> {
    let Some(st) = prev else {
        return Some(1); // never tried before
    };
    let quiet = now.saturating_duration_since(st.last_attempt);

    if quiet >= Duration::from_secs(RESPAWN_RESET_SECS) {
        return Some(1); // long enough since we last tried: forgive the history
    }
    if st.attempts >= MAX_RESPAWN_ATTEMPTS {
        return None; // crash-looping — wait out the reset window
    }
    if quiet < respawn_backoff(st.attempts) {
        return None; // too soon after the last try
    }
    Some(st.attempts + 1)
}

// ── McpManager ───────────────────────────────────────────────────────────────

pub struct McpManager {
    pool:            Arc<SqlitePool>,
    servers:         RwLock<HashMap<String, Arc<dyn McpServerClient>>>,
    errors:          RwLock<HashMap<String, String>>,
    descriptions:    RwLock<HashMap<String, Option<String>>>,
    /// Per-server manifest-declared friendly tool names (`server → tool → title`),
    /// the authoritative override for a tool's UI display name (`tool_display_name`).
    /// Populated from each spec's `tool_titles` at connect, forgotten on stop.
    titles:          RwLock<HashMap<String, HashMap<String, String>>>,
    notification_tx: mpsc::UnboundedSender<McpNotification>,
    /// Feeds per-server diagnostic lines (stderr, `notifications/message`,
    /// lifecycle) to the `logs::log_consumer`, which writes `logs/mcp/<name>.log`.
    log_tx:          McpLogTx,
    /// Bridges server-initiated `elicitation/create` requests to the Inbox.
    /// Set once via `set_elicitation_handler` before `initialize` runs.
    elicitation_handler: RwLock<Option<Arc<dyn ElicitationHandler>>>,
    /// Data root for persisting non-text tool-result media (`media_dir`).
    data_root:       PathBuf,
    /// The spec each running server was started from, kept so a dead one can be
    /// respawned without going back to the DB — which the manager could not do
    /// anyway for a per-user connector, whose row needs the OAuth credential
    /// re-resolved against the registry (`user_row_spec_resolved`).
    ///
    /// A spec carries the connector's live credential in `config.env` (an OAuth
    /// refresh token, an API key). That is the same RAM the child process already
    /// holds it in, on a per-user manager whose whole lifetime is one unlocked
    /// session (§9) — it neither widens the blast radius nor outlives the DEK.
    specs:           RwLock<HashMap<String, McpServerSpec>>,
    /// Restart bookkeeping per server. Empty until something actually dies.
    respawns:        RwLock<HashMap<String, RespawnState>>,
    /// Serializes respawns across the whole manager. Async because a respawn spans
    /// a process spawn and an `initialize` round-trip.
    respawn_lock:    tokio::sync::Mutex<()>,
}

/// Whether a runtime's server-pushed notifications are persisted to `mcp_events`.
///
/// `mcp_events` is an **owner** table and its only consumer is event triage, which is
/// per-user: an event is something that happened to *someone*. The global
/// runtime has no owner — its pool is `system.db` — so persisting there would
/// produce rows nobody can attribute and nobody will ever read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventLog {
    /// Per-user runtime: notifications land in that user's `mcp_events`.
    Persist,
    /// Global runtime: notifications are dropped after the diagnostic log line.
    Discard,
}

impl McpManager {
    pub fn new(
        pool:      Arc<SqlitePool>,
        shutdown:  CancellationToken,
        data_root: impl Into<PathBuf>,
        event_log: EventLog,
    ) -> Self {
        let (notification_tx, notification_rx) = mpsc::unbounded_channel::<McpNotification>();
        let (log_tx, log_rx) = mpsc::unbounded_channel::<McpLogLine>();

        let pool_bg = pool.clone();
        match event_log {
            EventLog::Persist => {
                tokio::spawn(Self::notification_consumer(pool_bg, notification_rx, shutdown.clone()));
            }
            // Still drain the channel: the senders are unbounded, but a receiver
            // dropped here would make every `send` fail and log noise per event.
            EventLog::Discard => {
                let sd = shutdown.clone();
                tokio::spawn(async move {
                    let mut rx = notification_rx;
                    loop {
                        tokio::select! {
                            _ = sd.cancelled() => break,
                            msg = rx.recv() => if msg.is_none() { break },
                        }
                    }
                });
            }
        }
        tokio::spawn(logs::log_consumer(log_rx, shutdown));

        Self {
            pool,
            servers:      RwLock::new(HashMap::new()),
            errors:       RwLock::new(HashMap::new()),
            descriptions: RwLock::new(HashMap::new()),
            titles:       RwLock::new(HashMap::new()),
            notification_tx,
            log_tx,
            elicitation_handler: RwLock::new(None),
            data_root:    data_root.into(),
            specs:        RwLock::new(HashMap::new()),
            respawns:     RwLock::new(HashMap::new()),
            respawn_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Emits a lifecycle line to a server's per-server log file (start failure,
    /// timeout, connection). Used for transports that have no `stderr` of their
    /// own to carry connection diagnostics (notably HTTP/SSE).
    fn log_lifecycle(&self, server: &str, text: impl Into<String>) {
        let _ = self.log_tx.send(McpLogLine::lifecycle(server.to_string(), text));
    }

    /// Directory under the data root where inline tool-result media (images,
    /// audio, embedded resources) is persisted and served from `/api/mcp-media/`.
    pub fn media_dir(&self) -> PathBuf {
        self.data_root.join("mcp_media")
    }

    /// Wire the elicitation bridge. Must be called before `initialize` so that
    /// stdio servers are started with a handler for `elicitation/create`.
    pub fn set_elicitation_handler(&self, handler: Arc<dyn ElicitationHandler>) {
        *self.elicitation_handler.write().unwrap() = Some(handler);
    }

    fn elicitation_handler(&self) -> Option<Arc<dyn ElicitationHandler>> {
        self.elicitation_handler.read().unwrap().clone()
    }

    async fn notification_consumer(
        pool:     Arc<SqlitePool>,
        mut rx:   mpsc::UnboundedReceiver<McpNotification>,
        shutdown: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("mcp: notification consumer shutdown");
                    break;
                }
                msg = rx.recv() => match msg {
                    Some((source, payload)) => {
                        let method  = payload["method"].as_str().unwrap_or("unknown").to_string();
                        let params  = serde_json::to_string(&payload["params"]).unwrap_or_else(|_| "{}".to_string());
                        match crate::db::mcp_events::insert(&pool, &source, &method, &params).await {
                            Ok(id) => info!("mcp_event stored: id={id} source={source} method={method}"),
                            Err(e) => warn!("mcp_events insert failed (source={source} method={method}): {e}"),
                        }
                    }
                    None => break,
                }
            }
        }
    }

    async fn start_one(
        cfg: &McpServerConfig,
        notification_tx: Option<mpsc::UnboundedSender<McpNotification>>,
        log_tx: Option<McpLogTx>,
        elicitation_handler: Option<Arc<dyn ElicitationHandler>>,
    ) -> Result<Arc<dyn McpServerClient>> {
        match cfg.transport {
            McpTransport::Stdio => {
                // Elicitation and per-server diagnostic capture (stderr +
                // notifications/message) are stdio-only. HTTP/SSE has no stderr and
                // no async notification stream, so it only gets lifecycle lines,
                // emitted by the manager (see `log_lifecycle`).
                McpServer::start(cfg, notification_tx, log_tx, elicitation_handler).await
                    .map(|s| Arc::new(s) as Arc<dyn McpServerClient>)
            }
            McpTransport::Http | McpTransport::Sse => {
                McpHttpServer::start(cfg).await
                    .map(|s| Arc::new(s) as Arc<dyn McpServerClient>)
            }
        }
    }

    /// Connects the GLOBAL runtime at boot: reads the enabled globally-active
    /// connectors (`mcp_global_servers`, host transport) and connects to them.
    /// The per-user runtime (blueprint §7/§9) is built separately at login and
    /// shares [`connect_all`] rather than this table-bound entry point.
    pub async fn initialize(&self) {
        let rows = match crate::db::mcp_global_servers::all_enabled(&self.pool).await {
            Ok(r) => r,
            Err(e) => { warn!("McpManager::initialize: failed to read DB: {e}"); return; }
        };

        if rows.is_empty() {
            info!("No enabled global MCP servers in DB — global MCP disabled.");
            crate::boot::section("MCP servers — none enabled");
            return;
        }

        let specs = rows.iter().map(global_row_spec).collect();
        self.connect_all(specs, true).await;
    }

    /// Connects to a batch of servers in parallel (each bounded by
    /// `SERVER_START_TIMEOUT_SECS`), recording their tools, errors and prompt
    /// descriptions. The reusable core shared by the global runtime
    /// ([`initialize`]) and the per-user runtime (built at login, §7). `boot`
    /// gates the curated boot-console lines, which only make sense at startup —
    /// a login-time per-user connect passes `false`.
    pub async fn connect_all(&self, specs: Vec<McpServerSpec>, boot: bool) {
        if specs.is_empty() {
            return;
        }
        {
            let mut descs = self.descriptions.write().unwrap();
            let mut titles = self.titles.write().unwrap();
            let mut kept  = self.specs.write().unwrap();
            for spec in &specs {
                descs.insert(spec.config.name.clone(), spec.description.clone());
                titles.insert(spec.config.name.clone(), spec.tool_titles.clone());
                // Remembered even if the start below fails: a server that never
                // came up is exactly one the sweep should keep trying, under the
                // same ceiling as one that came up and died.
                kept.insert(spec.config.name.clone(), spec.clone());
            }
        }
        if boot {
            crate::boot::section(format!(
                "MCP servers — connecting to {} in background", specs.len()
            ));
        }
        let handles: Vec<_> = specs.into_iter().map(|spec| {
            let cfg    = spec.config;
            let tx     = self.notification_tx.clone();
            let log_tx = self.log_tx.clone();
            let eh = self.elicitation_handler();
            tokio::spawn(async move {
                info!("MCP server '{}': starting…", cfg.name);
                let result = tokio::time::timeout(
                    Duration::from_secs(SERVER_START_TIMEOUT_SECS),
                    Self::start_one(&cfg, Some(tx), Some(log_tx), eh),
                ).await;
                (cfg.name, result)
            })
        }).collect();

        for handle in handles {
            match handle.await {
                Ok((name, Ok(Ok(s)))) => {
                    let tool_names: Vec<_> = s.tools().iter().map(|t| t.name.clone()).collect();
                    let n = tool_names.len();
                    info!("MCP server '{}' ready — {n} tool(s): {}", name, tool_names.join(", "));
                    if boot {
                        crate::boot::ok(format!("{name} ({n} tool{})", if n == 1 { "" } else { "s" }));
                    }
                    self.log_lifecycle(&name, format!("connected — {n} tool(s)"));
                    self.errors.write().unwrap().remove(&name);
                    self.servers.write().unwrap().insert(name, s);
                }
                Ok((name, Ok(Err(e)))) => {
                    warn!("MCP server '{}' failed to start: {e}", name);
                    if boot { crate::boot::fail(format!("{name} — {e}")); }
                    self.log_lifecycle(&name, format!("failed to start: {e}"));
                    self.errors.write().unwrap().insert(name, e.to_string());
                }
                Ok((name, Err(_))) => {
                    let msg = format!("startup timed out after {SERVER_START_TIMEOUT_SECS}s");
                    warn!("MCP server '{}' {msg}", name);
                    if boot { crate::boot::fail(format!("{name} — {msg}")); }
                    self.log_lifecycle(&name, &msg);
                    self.errors.write().unwrap().insert(name, msg);
                }
                Err(e) => { warn!("MCP startup task panicked: {e}"); }
            }
        }
    }

    /// Starts (or restarts) a single server from a spec and records it in the
    /// runtime maps. The DB write is the caller's job (the Connectors activation
    /// API) — this only touches the live connections. Returns the tool names.
    pub async fn start_server(&self, spec: McpServerSpec) -> Result<Vec<String>> {
        let name = spec.config.name.clone();
        self.specs.write().unwrap().insert(name.clone(), spec.clone());
        let client = tokio::time::timeout(
            Duration::from_secs(SERVER_START_TIMEOUT_SECS),
            Self::start_one(&spec.config, Some(self.notification_tx.clone()), Some(self.log_tx.clone()), self.elicitation_handler()),
        ).await
        .map_err(|_| {
            self.log_lifecycle(&name, "timed out during connection");
            anyhow::anyhow!("MCP server '{name}' timed out during connection")
        })?
        .map_err(|e| {
            self.log_lifecycle(&name, format!("failed to start: {e}"));
            anyhow::anyhow!("MCP server '{name}' failed to start: {e}")
        })?;

        let tool_names: Vec<String> = client.tools().iter().map(|t| t.name.clone()).collect();
        self.log_lifecycle(&name, format!("connected — {} tool(s)", tool_names.len()));
        self.errors.write().unwrap().remove(&name);
        self.descriptions.write().unwrap().insert(name.clone(), spec.description);
        self.titles.write().unwrap().insert(name.clone(), spec.tool_titles);
        self.servers.write().unwrap().insert(name, client);
        Ok(tool_names)
    }

    /// Stops a running server (dropping the client → `kill_on_drop`) and forgets
    /// it. DB removal is the caller's responsibility.
    pub fn stop_server(&self, name: &str) {
        self.servers.write().unwrap().remove(name);
        self.errors.write().unwrap().remove(name);
        self.descriptions.write().unwrap().remove(name);
        self.titles.write().unwrap().remove(name);
        // Forgetting the spec is what makes a stop a stop: leaving it would let the
        // sweep resurrect a connector the admin just revoked or deactivated.
        self.specs.write().unwrap().remove(name);
        self.respawns.write().unwrap().remove(name);
    }

    /// Stops **every** running server (each dropped client → `kill_on_drop` kills
    /// its child process) and forgets them. Used when a per-user container is
    /// recreated (§6 remount): the old `docker exec -i` children are bound to the
    /// now-gone container, so they must be torn down before reconnecting against
    /// the fresh one via [`connect_all`](Self::connect_all).
    pub fn stop_all(&self) {
        self.servers.write().unwrap().clear();
        self.errors.write().unwrap().clear();
        self.descriptions.write().unwrap().clear();
        self.titles.write().unwrap().clear();
        // Same reason as `stop_server`, and load-bearing for the container remount
        // this is called from: the specs name a container that is being replaced, so
        // respawning against them would spawn into the one that just went away.
        // `connect_all` re-populates with specs built for the new container.
        self.specs.write().unwrap().clear();
        self.respawns.write().unwrap().clear();
    }

    /// Whether a server by this name currently has a live connection in the
    /// runtime. Used by the interactive-login API to decide whether it must
    /// (re)start a pending connector before polling its `login_status`.
    pub fn is_running(&self, name: &str) -> bool {
        self.servers.read().unwrap().contains_key(name)
    }

    // ── Respawn ──────────────────────────────────────────────────────────────

    /// Whether `name` is a server we are supposed to be running but currently are
    /// not: it has a remembered spec, and either no handle at all (a start that
    /// failed) or a handle whose child process has exited.
    ///
    /// A server with no spec is never "dead" — it was stopped on purpose, or is a
    /// handle registered by some path that does not want supervision.
    fn is_dead(&self, name: &str) -> bool {
        let supervised = self.specs.read().unwrap().contains_key(name);
        if !supervised {
            return false;
        }
        match self.servers.read().unwrap().get(name) {
            Some(s) => !s.is_alive(),
            None    => true,
        }
    }

    /// The supervised servers that are currently down.
    fn dead_servers(&self) -> Vec<String> {
        // Names are collected before the `servers` lock is taken so the two guards
        // never overlap — every other reader here takes them in this order too.
        let names: Vec<String> = self.specs.read().unwrap().keys().cloned().collect();
        let servers = self.servers.read().unwrap();
        names.into_iter()
            .filter(|n| servers.get(n).map(|s| !s.is_alive()).unwrap_or(true))
            .collect()
    }

    /// Takes one unit of restart budget for `name`, or refuses.
    ///
    /// Refusing never sleeps — see [`RESPAWN_BACKOFF_SECS`]. The caller either has a
    /// user waiting (fail now, honestly) or is the sweep (come back in a few
    /// seconds), and neither is improved by parking a task on a timer.
    fn claim_respawn(&self, name: &str) -> bool {
        let now = std::time::Instant::now();
        let mut states = self.respawns.write().unwrap();
        match respawn_decision(states.get(name).copied(), now) {
            Some(attempts) => {
                states.insert(name.to_string(), RespawnState { attempts, last_attempt: now });
                true
            }
            None => false,
        }
    }

    /// Restarts `name` if its process is gone. Returns whether it is alive on exit,
    /// so a caller about to use the server knows whether the repair worked.
    ///
    /// Best-effort by contract: a refusal (budget spent, spec forgotten, spawn
    /// failed) leaves the manager exactly as it was and the caller's own error path
    /// takes over. Nothing here is on an authorization path — the spec was already
    /// access-checked when it was built, and `stop_server` drops it, so a revoked
    /// connector has nothing to respawn from.
    pub async fn restart_if_dead(&self, name: &str) -> bool {
        if !self.is_dead(name) {
            return true;
        }
        // One respawn at a time. A parallel tool batch can find the same server
        // dead in several calls at once; without this each would spawn a child and
        // all but the last would be orphaned in the map, still running.
        let _guard = self.respawn_lock.lock().await;
        // Re-check under the lock: whoever held it may have just fixed this one.
        if !self.is_dead(name) {
            return true;
        }
        if !self.claim_respawn(name) {
            return false;
        }
        let spec = self.specs.read().unwrap().get(name).cloned();
        let Some(spec) = spec else { return false };

        warn!("MCP server '{name}' is not running — restarting it");
        self.log_lifecycle(name, "process gone — restarting");
        // Drop the dead handle first so its `kill_on_drop` reaps anything left of
        // the old child before a new one claims the same connector directory.
        self.servers.write().unwrap().remove(name);

        match self.start_server(spec).await {
            Ok(tools) => {
                info!("MCP server '{name}' restarted — {} tool(s)", tools.len());
                true
            }
            Err(e) => {
                warn!("MCP server '{name}' restart failed: {e}");
                self.errors.write().unwrap().insert(name.to_string(), e.to_string());
                false
            }
        }
    }

    /// Starts the background sweep that restarts servers which died while nobody
    /// was calling them.
    ///
    /// Separate from a call-time repair because the two answer different failures.
    /// A tool call repairs the connector it was about to use, which is enough for a
    /// connector that only ever acts when asked. It is not enough for one that
    /// *pushes* — Gmail's poll thread produces the `event/new_email` notifications
    /// that feed event triage, and after a crash those simply stop, with no call to
    /// notice and nothing in the UI to say so. The sweep is what bounds that
    /// silence to [`RESPAWN_SWEEP_SECS`].
    ///
    /// Holds a `Weak`: a per-user manager dies with its `UserContext` at logout, and
    /// an `Arc` here would keep that runtime — and its `docker exec` children —
    /// alive past the moment the user's key left RAM (§9).
    pub fn spawn_respawn_sweep(
        self: &Arc<Self>,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(RESPAWN_SWEEP_SECS));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        let Some(mgr) = weak.upgrade() else { break };
                        for name in mgr.dead_servers() {
                            mgr.restart_if_dead(&name).await;
                        }
                    }
                }
            }
        })
    }

    pub fn tools(&self) -> Vec<McpTool> {
        self.servers.read().unwrap().values()
            .flat_map(|s| s.tools().iter().cloned())
            .collect()
    }

    pub fn tools_for(&self, names: &[String]) -> Vec<McpTool> {
        self.servers.read().unwrap().iter()
            .filter(|(name, _)| names.contains(name))
            .flat_map(|(_, s)| s.tools().iter().cloned())
            .collect()
    }

    /// Best friendly name for a tool for UI display: the manifest-declared override
    /// (`tool_titles`) wins, else the server's live MCP `title` (2025-06-18+), else
    /// `None` — the caller falls back to a prettified raw name. Cheap: an O(tools)
    /// scan per call, run once per tool-call event.
    pub fn tool_display_name(&self, server: &str, tool: &str) -> Option<String> {
        if let Some(t) = self.titles.read().unwrap().get(server).and_then(|m| m.get(tool)) {
            return Some(t.clone());
        }
        self.servers.read().unwrap().get(server)
            .and_then(|s| s.tools().iter().find(|t| t.name == tool).and_then(|t| t.title.clone()))
    }

    pub fn server_descriptions(&self) -> HashMap<String, Option<String>> {
        self.descriptions.read().unwrap().clone()
    }

    pub fn server_infos(&self) -> Vec<Value> {
        self.servers.read().unwrap().iter()
            .map(|(name, s)| json!({
                "name": name,
                "tools": s.tools().iter().map(|t| json!({
                    "name":        t.name,
                    "description": t.description,
                })).collect::<Vec<_>>(),
            }))
            .collect()
    }

    pub async fn call(&self, server: &str, tool: &str, args: Value) -> Result<ToolResult> {
        // Repair a connector that died since the last call, before using it. This is
        // the path the user actually feels: a crashed server used to turn every
        // subsequent tool call into "MCP '<name>' disconnected" until the next
        // login. A no-op (and one map read) whenever the server is healthy.
        self.restart_if_dead(server).await;
        let s = self.servers.read().unwrap()
            .get(server)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server}' not found"))?;
        match s.call_tool(tool, args).await? {
            McpCallResult::Text(t) => Ok(ToolResult::Text(t)),
            McpCallResult::Json(v) => Ok(ToolResult::Json(v)),
            McpCallResult::Media { text, structured, items } =>
                Ok(ToolResult::Text(self.persist_media(server, text, structured, items).await)),
            // Experimental Tasks — defensive fallback. Normally the transport's
            // `call_tool` polls a deferred task to completion (block-and-poll) and
            // returns the real result, so this arm is not hit. It only surfaces a
            // raw handle if polling was bypassed, so the result is never lost.
            McpCallResult::Task(t) => {
                let ttl = t.ttl_ms.map(|ms| format!(", ttl {}s", ms / 1000)).unwrap_or_default();
                Ok(ToolResult::Text(format!(
                    "MCP server '{server}' deferred this call as task `{}` (status: {:?}{ttl}). \
                     Task polling is not implemented yet, so the result can't be retrieved automatically.",
                    t.task_id, t.status,
                )))
            }
        }
    }

    /// Persists the inline media of an MCP tool result under [`media_dir`] and
    /// composes a markdown text result that references each item by URL — so the
    /// model can surface it (the frontend renders the markdown) instead of the
    /// bytes being silently dropped. `resource_link`s are passed through by URI
    /// without downloading. Falls back to a textual placeholder if a write fails,
    /// so a disk error never loses the rest of the result.
    async fn persist_media(
        &self,
        server:     &str,
        text:       Option<String>,
        structured: Option<Value>,
        items:      Vec<McpMedia>,
    ) -> String {
        let mut out: Vec<String> = Vec::new();
        if let Some(t) = text.filter(|t| !t.is_empty()) {
            out.push(t);
        }

        for item in items {
            match item.data {
                McpMediaData::Inline { bytes, mime } => {
                    let file = format!("{}.{}", random_id(), ext_for_mime(&mime));
                    let dir  = self.media_dir();
                    let saved = async {
                        tokio::fs::create_dir_all(&dir).await?;
                        tokio::fs::write(dir.join(&file), &bytes).await
                    }.await;
                    match saved {
                        Ok(()) => {
                            let url = format!("/api/mcp-media/{file}");
                            let kb  = bytes.len().div_ceil(1024);
                            out.push(match item.kind {
                                McpMediaKind::Image    => format!("![image]({url}) ({mime}, {kb} KB)"),
                                McpMediaKind::Audio    => format!("[audio]({url}) ({mime}, {kb} KB)"),
                                McpMediaKind::Resource => format!("[file]({url}) ({mime}, {kb} KB)"),
                            });
                        }
                        Err(e) => {
                            warn!("MCP '{server}': failed to persist tool-result media: {e}");
                            out.push(format!("[media not saved: {mime}]"));
                        }
                    }
                }
                McpMediaData::Link { uri, mime } => {
                    let label = mime.as_deref().unwrap_or("resource");
                    out.push(format!("[{label}]({uri})"));
                }
            }
        }

        if let Some(sc) = structured {
            if let Ok(s) = serde_json::to_string_pretty(&sc) {
                out.push(format!("```json\n{s}\n```"));
            }
        }

        out.join("\n\n")
    }
}

/// A server to connect: its transport config plus the description shown in the
/// "Available MCP servers" prompt section. Decouples [`McpManager`] from any DB
/// table — the global and per-user runtimes each build these from their own rows
/// (`global_row_spec` / `user_row_spec`).
#[derive(Clone)]
pub struct McpServerSpec {
    pub config:      McpServerConfig,
    pub description: Option<String>,
    /// Manifest-declared friendly tool names (`tool name → display title`), the
    /// authoritative override for a connector's UI display names. Empty for globals
    /// and for per-user rows with no catalog `tool_meta_json`; the runtime then
    /// falls back to the server's live MCP `title` and finally a prettified name.
    pub tool_titles: HashMap<String, String>,
}

fn transport_of(s: &str) -> McpTransport {
    match s {
        "http" => McpTransport::Http,
        "sse"  => McpTransport::Sse,
        _      => McpTransport::Stdio,
    }
}

/// Some remote MCP servers take their credentials as **query parameters** rather
/// than the `Authorization: Bearer` header this client sends by default (Tavily
/// wants `?tavilyApiKey=…`). Those declare placeholders in their URL, substituted
/// here — at connect time, in memory.
///
/// Three placeholder forms are understood:
/// - `{key}` — the legacy single-key form: replaced with `api_key`.
/// - `{SECRET:name}` / `{ENV:name}` — the **named** form: replaced with the
///   connector's own form field `env[name]`. This is what lets a remote connector's
///   URL carry more than one credential (`?key={SECRET:apiKey}&region={ENV:region}`),
///   each supplied by a described `env[]` entry.
/// - `{SECRET:name}` with no matching `env[name]` — the pre-schema form (a row
///   written before the feed moved a connector's key into `env[]`): falls back to
///   `api_key`, so an old row still connects.
///
/// Resolving here rather than at write time keeps a per-user secret in the encrypted
/// `{userid}.db` (`env_json` / the api_key column) instead of baking a live secret
/// into a stored URL. `api_key` is cleared once it has been spent on the URL so it is
/// not also sent as a bearer header the server never asked for.
fn apply_key_placeholder(
    url:     Option<String>,
    api_key: Option<String>,
    env:     &HashMap<String, String>,
) -> (Option<String>, Option<String>) {
    let url = match url {
        Some(u) => u,
        None => return (None, api_key),
    };

    let mut api_key_spent = false;

    // Legacy single-key placeholder first (no name to resolve).
    let url = if url.contains("{key}") {
        match &api_key {
            Some(k) => { api_key_spent = true; url.replace("{key}", k) }
            None    => url,
        }
    } else {
        url
    };

    let url = substitute_named_tokens(&url, env, api_key.as_deref(), &mut api_key_spent);
    let api_key = if api_key_spent { None } else { api_key };
    (Some(url), api_key)
}

/// Replaces `{SECRET:name}` / `{ENV:name}` tokens in `text`, each looked up by name
/// in `env`. A `{SECRET:name}` with no matching `env` entry falls back to `api_key`
/// (setting `api_key_spent`) — the pre-schema single-key form. A token that resolves
/// to nothing is left in place, so a misconfiguration is a visible, debuggable URL
/// rather than a silently-wrong one. Non-`SECRET:`/`ENV:` braces (e.g. a stray
/// `{key}` when no api_key was set) are left untouched.
fn substitute_named_tokens(
    text:          &str,
    env:           &HashMap<String, String>,
    api_key:       Option<&str>,
    api_key_spent: &mut bool,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        let Some(close) = after.find('}') else {
            out.push_str(after);
            return out;
        };
        let token = &after[1..close];
        let resolved = if let Some(name) = token.strip_prefix("SECRET:") {
            match env.get(name) {
                Some(v) => Some(v.clone()),
                None    => api_key.map(|k| { *api_key_spent = true; k.to_string() }),
            }
        } else if let Some(name) = token.strip_prefix("ENV:") {
            env.get(name).cloned()
        } else {
            None
        };
        match resolved {
            Some(v) => out.push_str(&v),
            None    => out.push_str(&after[..=close]), // leave the `{…}` literally
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Builds a spec for a globally-active connector — host transport (`launch_in`
/// = None), so it runs in the Skald process, not in any container (§7).
pub fn global_row_spec(row: &crate::db::mcp_global_servers::McpGlobalServerRow) -> McpServerSpec {
    let mut env = row.env();
    // A `global` python `local_script` connector's deps are installed on the host
    // under `<dir>/.pydeps` (see `install::ensure_installed_host`); point the
    // interpreter at them, mirroring `user_row_spec`. `args()[0]` is the host-absolute
    // script path (set by `global_enable`), so the derived `.pydeps` path is absolute
    // too and resolves regardless of the process cwd. A no-op for a remote connector
    // (no python command → None) or before the first install (python ignores a
    // missing `PYTHONPATH` entry).
    if let Some(pp) = python_pydeps_path(row.command.as_deref(), &row.args()) {
        env.entry("PYTHONPATH".to_string()).or_insert(pp);
    }
    let (url, api_key) = apply_key_placeholder(row.url.clone(), row.api_key.clone(), &env);
    McpServerSpec {
        config: McpServerConfig {
            name:      row.name.clone(),
            transport: transport_of(&row.transport),
            command:   row.command.clone(),
            args:      Some(row.args()).filter(|v| !v.is_empty()),
            env:       Some(env).filter(|m| !m.is_empty()),
            url,
            api_key,
            launch_in: None,
        },
        description: row.description.clone(),
        tool_titles: HashMap::new(),
    }
}

/// The `PYTHONPATH` to hand a python connector so it imports the deps installed
/// under `<connector-dir>/.pydeps`. `None` for anything that is not a python
/// command, or a python one with no script argument to derive the dir from.
fn python_pydeps_path(command: Option<&str>, args: &[String]) -> Option<String> {
    let cmd = command?;
    let base = std::path::Path::new(cmd).file_name().and_then(|s| s.to_str()).unwrap_or(cmd);
    if !base.starts_with("python") {
        return None;
    }
    let script = args.first()?;
    let dir = std::path::Path::new(script).parent()?;
    Some(dir.join(install::PYDEPS_DIR).to_string_lossy().into_owned())
}

/// Reconciles a per-user local-script connector's files + dependencies inside the
/// user's container before it is (re)started — see [`install::ensure_installed`].
/// Best-effort: logs and returns on any failure so a broken connector never blocks
/// the others from starting. A no-op for remote / self-registered rows (no vetted
/// catalog folder to install from).
pub async fn prepare_local_connector(
    registry:  &SqlitePool,
    user_id:   &str,
    container: &str,
    row:       &crate::db::mcp_user_servers::McpUserServerRow,
) {
    if row.source != "local_script" {
        return;
    }
    let Some(catalog_name) = row.catalog_name.as_deref() else { return };
    let folder = match crate::db::mcp_catalog::get_by_name(registry, catalog_name).await {
        Ok(Some(entry)) => entry
            .script_path
            .as_deref()
            .and_then(|sp| install::split_script_path(sp).ok())
            .map(|(folder, _)| folder.to_string()),
        Ok(None) => None,
        Err(e) => {
            warn!("connector '{}': catalog lookup failed: {e}", row.name);
            None
        }
    };
    let Some(folder) = folder else { return };
    if let Err(e) = install::ensure_installed(user_id, &row.name, &folder, container).await {
        warn!("connector '{}': dependency install failed: {e}", row.name);
    }
}

/// Builds a spec for a user's per-user connector — container transport: a
/// `local_script` (or any stdio server) runs INSIDE the user's container
/// (`launch_in = Some(container)`), against the script copied into the
/// bind-mounted home. Remote (HTTP) connectors ignore `launch_in`.
pub fn user_row_spec(
    row:       &crate::db::mcp_user_servers::McpUserServerRow,
    container: &str,
) -> McpServerSpec {
    let transport = transport_of(&row.transport);
    let launch_in = matches!(transport, McpTransport::Stdio).then(|| container.to_string());
    let mut env = row.env();
    // A python connector's deps are installed under `<dir>/.pydeps` (pip `--target`,
    // see `install::ensure_installed`); point the interpreter at them. Node needs
    // nothing — `node_modules/` beside the entry file resolves on its own. Setting
    // it unconditionally for a python command is safe even before the first install:
    // python silently ignores a non-existent `PYTHONPATH` entry.
    if let Some(pp) = python_pydeps_path(row.command.as_deref(), &row.args()) {
        env.entry("PYTHONPATH".to_string()).or_insert(pp);
    }
    let (url, api_key) = apply_key_placeholder(row.url.clone(), row.api_key.clone(), &env);
    McpServerSpec {
        config: McpServerConfig {
            name:      row.name.clone(),
            transport,
            command:   row.command.clone(),
            args:      Some(row.args()).filter(|v| !v.is_empty()),
            env:       Some(env).filter(|m| !m.is_empty()),
            url,
            api_key,
            launch_in,
        },
        // A per-user connector's description falls back to its catalog name; the
        // catalog's friendly description can be injected by the caller if richer.
        description: row.catalog_name.clone(),
        // Manifest tool titles are loaded from the catalog by `user_row_spec_resolved`,
        // which has registry access; the bare sync builder leaves them empty.
        tool_titles: HashMap::new(),
    }
}

/// Like [`user_row_spec`], but for an OAuth connector it also resolves the stored
/// refresh token into the credential the server reads and injects it into the
/// process env per the delivery spec (§15). Non-OAuth rows are returned unchanged.
///
/// `registry` is the system pool, where `oauth_providers` (the client credentials)
/// lives. A resolution failure is logged, not fatal: the server still starts, and
/// fails its own auth visibly, rather than the whole login batch aborting.
pub async fn user_row_spec_resolved(
    row:       &crate::db::mcp_user_servers::McpUserServerRow,
    container: &str,
    registry:  &SqlitePool,
) -> McpServerSpec {
    let mut spec = user_row_spec(row, container);
    // Manifest-declared friendly tool names (UI card titles): snapshot the catalog's
    // `tool_meta_json` for this activation so `tool_display_name` can override the raw
    // name. Best-effort — a missing/failed lookup just leaves the live `title` path.
    if let Some(catalog_name) = row.catalog_name.as_deref() {
        if let Ok(Some(entry)) = crate::db::mcp_catalog::get_by_name(registry, catalog_name).await {
            spec.tool_titles = crate::db::mcp_catalog::parse_tool_titles(entry.tool_meta_json.as_deref());
            // The catalog's `description` is the connector's `llm_short_description` —
            // the line the model reads when deciding whether to `activate_tools()` on
            // this server (see `render_mcp_list`). `user_row_spec` only had the bare
            // catalog name to fall back on; inject the real blurb here (this is the
            // "the caller injects it if richer" the sync builder defers to), and keep
            // the name when the catalog has none. Because it is read from the catalog
            // live, a marketplace reinstall that rewrites the description is reflected
            // the next time this spec is built.
            if let Some(desc) = entry.description {
                spec.description = Some(desc);
            }
        }
    }
    if let (Some(provider), Some(deliver), Some(refresh)) =
        (row.oauth_provider.as_deref(), row.deliver(), row.api_key.as_deref())
    {
        if let Err(e) = inject_oauth_env(&mut spec, provider, &deliver, refresh, registry).await {
            warn!("connector '{}': OAuth credential delivery failed: {e}", row.name);
        }
    }
    spec
}

/// Assembles the credential from the provider's client creds + the refresh token and
/// sets it on `spec.config.env` under the delivery spec's env name.
async fn inject_oauth_env(
    spec:          &mut McpServerSpec,
    provider_name: &str,
    deliver:       &DeliverSpec,
    refresh_token: &str,
    registry:      &SqlitePool,
) -> Result<()> {
    if deliver.as_ != "env" {
        anyhow::bail!("only `env` credential delivery is wired (deliver.as = `{}`)", deliver.as_);
    }
    let env_name = deliver.env.as_deref()
        .ok_or_else(|| anyhow::anyhow!("deliver.as=env but no deliver.env name"))?;
    let format = deliver.format.as_deref().unwrap_or("google_authorized_user");
    let provider = crate::db::oauth_providers::get(registry, provider_name).await?
        .ok_or_else(|| anyhow::anyhow!("unknown OAuth provider `{provider_name}`"))?;
    let cred = oauth::assemble_credential(format, &provider, refresh_token)?;
    spec.config.env.get_or_insert_with(HashMap::new).insert(env_name.to_string(), cred);
    Ok(())
}

/// Generates a 32-char alphanumeric id for a persisted media filename
/// (mirrors `ImageGeneratorManager`).
fn random_id() -> String {
    rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

/// Maps a MIME type to a file extension for persisted MCP media; `bin` for unknown.
pub fn ext_for_mime(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or("").trim() {
        "image/png"        => "png",
        "image/jpeg"       => "jpg",
        "image/gif"        => "gif",
        "image/webp"       => "webp",
        "image/svg+xml"    => "svg",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mpeg"       => "mp3",
        "audio/ogg"        => "ogg",
        "video/mp4"        => "mp4",
        "video/webm"       => "webm",
        "application/pdf"  => "pdf",
        "application/json" => "json",
        "text/plain"       => "txt",
        _                  => "bin",
    }
}

/// Inverse of [`ext_for_mime`] for serving persisted media with the right
/// `Content-Type`; generic binary for unknown extensions.
pub fn content_type_for_ext(ext: &str) -> &'static str {
    match ext {
        "png"  => "image/png",
        "jpg"  => "image/jpeg",
        "gif"  => "image/gif",
        "webp" => "image/webp",
        "svg"  => "image/svg+xml",
        "wav"  => "audio/wav",
        "mp3"  => "audio/mpeg",
        "ogg"  => "audio/ogg",
        "mp4"  => "video/mp4",
        "webm" => "video/webm",
        "pdf"  => "application/pdf",
        "json" => "application/json",
        "txt"  => "text/plain",
        _      => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn key_placeholder_legacy_is_substituted() {
        let (url, key) = apply_key_placeholder(
            Some("https://x/?k={key}".into()),
            Some("secret123".into()),
            &HashMap::new(),
        );
        assert_eq!(url.as_deref(), Some("https://x/?k=secret123"));
        assert!(key.is_none(), "api_key is consumed after substitution");
    }

    #[test]
    fn key_placeholder_secret_token_falls_back_to_api_key() {
        // Pre-schema Tavily: the key sits in the api_key column and the URL names
        // {SECRET:tavilyApiKey}, but env carries no such entry → fall back to api_key.
        let (url, key) = apply_key_placeholder(
            Some("https://mcp.tavily.com/mcp/?tavilyApiKey={SECRET:tavilyApiKey}".into()),
            Some("tvly-abc".into()),
            &HashMap::new(),
        );
        assert_eq!(url.as_deref(), Some("https://mcp.tavily.com/mcp/?tavilyApiKey=tvly-abc"));
        assert!(key.is_none(), "api_key is consumed when it was spent on the URL");
    }

    #[test]
    fn secret_token_resolves_from_env_by_name() {
        // New schema-driven Tavily: the key was typed into the described `env[]`
        // field `tavilyApiKey`, so it lives in env and there is no api_key column.
        let (url, key) = apply_key_placeholder(
            Some("https://mcp.tavily.com/mcp/?tavilyApiKey={SECRET:tavilyApiKey}".into()),
            None,
            &map(&[("tavilyApiKey", "tvly-xyz")]),
        );
        assert_eq!(url.as_deref(), Some("https://mcp.tavily.com/mcp/?tavilyApiKey=tvly-xyz"));
        assert!(key.is_none());
    }

    #[test]
    fn multiple_named_params_resolve_independently() {
        // The point of the named form: more than one credential in one URL, each
        // from its own form field (SECRET for secrets, ENV for the rest).
        let (url, key) = apply_key_placeholder(
            Some("https://api/mcp?key={SECRET:apiKey}&region={ENV:region}".into()),
            None,
            &map(&[("apiKey", "sk-1"), ("region", "us-east-1")]),
        );
        assert_eq!(url.as_deref(), Some("https://api/mcp?key=sk-1&region=us-east-1"));
        assert!(key.is_none());
    }

    #[test]
    fn unresolved_token_is_left_in_place() {
        // No env entry and no api_key to fall back to → the token stays, a visible
        // misconfiguration rather than a silently-wrong URL.
        let (url, key) = apply_key_placeholder(
            Some("https://api/mcp?key={SECRET:missing}".into()),
            None,
            &HashMap::new(),
        );
        assert_eq!(url.as_deref(), Some("https://api/mcp?key={SECRET:missing}"));
        assert!(key.is_none());
    }

    #[test]
    fn key_placeholder_no_token_keeps_key_for_bearer() {
        // No placeholder in the URL → the key stays, so the HTTP transport
        // sends it as `Authorization: Bearer`.
        let (url, key) = apply_key_placeholder(
            Some("https://x.example.com/mcp".into()),
            Some("bearer-key".into()),
            &HashMap::new(),
        );
        assert_eq!(url.as_deref(), Some("https://x.example.com/mcp"));
        assert_eq!(key.as_deref(), Some("bearer-key"));
    }

    #[test]
    fn substitute_named_tokens_prefers_env_over_api_key() {
        // A SECRET token whose name IS in env resolves from env; the api_key is left
        // untouched (it may still be needed as a bearer for the same connector).
        let mut spent = false;
        let s = substitute_named_tokens(
            "a={SECRET:K}&b={SECRET:K}&c={ENV:C}",
            &map(&[("K", "VAL"), ("C", "CC")]),
            Some("bearer"),
            &mut spent,
        );
        assert_eq!(s, "a=VAL&b=VAL&c=CC");
        assert!(!spent, "env satisfied the tokens, so api_key was not spent");
    }

    // ── Respawn policy ───────────────────────────────────────────────────────

    use std::time::Instant;

    /// A state whose last attempt was `secs` ago, as seen from `now`.
    fn attempted(attempts: u32, secs: u64, now: Instant) -> Option<RespawnState> {
        Some(RespawnState {
            attempts,
            last_attempt: now.checked_sub(Duration::from_secs(secs)).expect("test clock underflow"),
        })
    }

    #[test]
    fn first_death_restarts_immediately() {
        // The common case, and the one the user feels: a connector that has been up
        // for days segfaults once. There is nothing to back off from yet.
        let now = Instant::now();
        assert_eq!(respawn_decision(None, now), Some(1));
    }

    #[test]
    fn a_second_death_waits_out_the_backoff() {
        let now = Instant::now();
        // One second after attempt #1 — inside the 2s window.
        assert_eq!(respawn_decision(attempted(1, 1, now), now), None);
        // Past it.
        assert_eq!(respawn_decision(attempted(1, 5, now), now), Some(2));
    }

    #[test]
    fn backoff_grows_then_stops_growing() {
        assert_eq!(respawn_backoff(0), Duration::from_secs(2));
        assert_eq!(respawn_backoff(3), Duration::from_secs(16));
        // Capped, and no overflow panic for an attempt count that cannot occur but
        // must not be able to crash the sweep if it ever did.
        assert_eq!(respawn_backoff(40), Duration::from_secs(RESPAWN_BACKOFF_MAX_SECS));
    }

    #[test]
    fn a_crash_loop_is_given_up_on() {
        let now = Instant::now();
        // Budget spent, and the backoff would otherwise have elapsed.
        assert_eq!(respawn_decision(attempted(MAX_RESPAWN_ATTEMPTS, 120, now), now), None);
    }

    #[test]
    fn giving_up_is_not_permanent() {
        // The escape hatch: after a quiet window even an exhausted server is tried
        // once more, so a box left running recovers from a transient outage instead
        // of staying dark until the next login.
        let now = Instant::now();
        let quiet = RESPAWN_RESET_SECS + 1;
        assert_eq!(respawn_decision(attempted(MAX_RESPAWN_ATTEMPTS, quiet, now), now), Some(1));
    }

    #[test]
    fn recovery_forgives_the_attempt_history() {
        // A server that died twice, was restarted, and then ran fine for an hour
        // must not be judged on those two deaths when it eventually dies again.
        let now = Instant::now();
        assert_eq!(respawn_decision(attempted(2, 3600, now), now), Some(1));
    }
}
