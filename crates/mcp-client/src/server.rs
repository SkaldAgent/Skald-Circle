use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, warn};

use crate::{McpCallResult, McpLogLine, McpLogTx, McpServerClient, McpTool, extract_text, interpolate_env};
use crate::config::McpServerConfig;

/// A server-initiated notification from an MCP server: `(server_name, full JSON-RPC message)`.
pub type McpNotification = (String, Value);

const CALL_TIMEOUT_SECS: u64 = 120;

// ── Elicitation (MCP spec 2025-06-18) ──────────────────────────────────────────

/// A server-initiated elicitation request: the server asks the user for input
/// *during* a tool call (`elicitation/create`). The secret/value never reaches
/// the LLM and is never persisted.
#[derive(Debug, Clone)]
pub struct ElicitationRequest {
    /// Human-readable message to show the user.
    pub message:          String,
    /// JSON Schema (flat object of typed fields) describing the requested input.
    pub requested_schema: Value,
}

/// The user's decision on an elicitation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

impl ElicitationAction {
    fn as_str(self) -> &'static str {
        match self {
            ElicitationAction::Accept  => "accept",
            ElicitationAction::Decline => "decline",
            ElicitationAction::Cancel  => "cancel",
        }
    }
}

/// The reply sent back to the server for an `elicitation/create` request.
#[derive(Debug, Clone)]
pub struct ElicitationReply {
    pub action:  ElicitationAction,
    /// Field values for `accept`; `None` for `decline`/`cancel`.
    pub content: Option<Value>,
}

/// Bridges a server-initiated elicitation to whatever surfaces it to the user
/// (in Skald: the Agent Inbox via `ElicitationManager`). The crate writes the
/// JSON-RPC reply itself — the handler only produces the decision. The returned
/// value (and any secret it carries) flows straight to the server's stdin and is
/// never logged here.
#[async_trait]
pub trait ElicitationHandler: Send + Sync {
    async fn handle(&self, server_name: &str, request: ElicitationRequest) -> ElicitationReply;
}

/// Serialises `msg` as a single JSON-RPC line and writes it to the child's stdin.
async fn write_json_line(stdin: &Arc<Mutex<ChildStdin>>, msg: &Value) {
    if let Ok(mut line) = serde_json::to_string(msg) {
        line.push('\n');
        let _ = stdin.lock().await.write_all(line.as_bytes()).await;
    }
}

/// Handles a server→client JSON-RPC request (e.g. `elicitation/create`) without
/// blocking the read-loop: the user reply may take minutes, so the work is
/// spawned and the JSON-RPC response is written back when it resolves.
fn handle_server_request(
    server_name:          &str,
    msg:                  Value,
    stdin:                &Arc<Mutex<ChildStdin>>,
    handler:              &Option<Arc<dyn ElicitationHandler>>,
    pending_elicitations: &Arc<AtomicUsize>,
) {
    let id     = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");

    if method == "elicitation/create" {
        let params  = msg.get("params").cloned().unwrap_or_else(|| json!({}));
        let request = ElicitationRequest {
            message:          params.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
            requested_schema: params.get("requestedSchema").cloned().unwrap_or_else(|| json!({})),
        };
        let stdin = Arc::clone(stdin);

        let Some(handler) = handler.clone() else {
            // Capability declared but no handler wired: cancel so the server
            // doesn't hang waiting for input we can't collect.
            tokio::spawn(async move {
                write_json_line(&stdin, &json!({
                    "jsonrpc": "2.0", "id": id, "result": { "action": "cancel" },
                })).await;
            });
            return;
        };

        pending_elicitations.fetch_add(1, Ordering::SeqCst);
        let counter = Arc::clone(pending_elicitations);
        let server  = server_name.to_string();
        tokio::spawn(async move {
            let reply      = handler.handle(&server, request).await;
            let mut result = json!({ "action": reply.action.as_str() });
            if reply.action == ElicitationAction::Accept {
                if let Some(content) = reply.content {
                    result["content"] = content;
                }
            }
            write_json_line(&stdin, &json!({
                "jsonrpc": "2.0", "id": id, "result": result,
            })).await;
            counter.fetch_sub(1, Ordering::SeqCst);
        });
    } else {
        // Unknown server→client request: reply method-not-found so the server
        // isn't left hanging.
        let stdin  = Arc::clone(stdin);
        let method = method.to_string();
        tokio::spawn(async move {
            write_json_line(&stdin, &json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") },
            })).await;
        });
    }
}

/// Guards an in-flight `tools/call`: if this is dropped while still armed — the
/// caller aborted the tool (a `/stop` drops the work future) or the call timed out
/// — it tells the server to stop via `notifications/cancelled` and drops the now
/// orphaned `pending` entry (which would otherwise leak). Disarmed once the server
/// replies or disconnects, where cancelling is pointless. The spec forbids
/// cancelling `initialize`, so only `tools/call` arms a guard.
struct CancelOnDrop {
    id:      u64,
    stdin:   Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    name:    String,
    reason:  &'static str,
    armed:   bool,
}

impl CancelOnDrop {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (id, stdin, pending, name, reason) =
            (self.id, Arc::clone(&self.stdin), Arc::clone(&self.pending), self.name.clone(), self.reason);
        tokio::spawn(async move {
            pending.lock().await.remove(&id);
            debug!(target: "mcp_client", "[{name}] notifications/cancelled for request {id} ({reason})");
            write_json_line(&stdin, &crate::cancelled_notification(id, reason)).await;
        });
    }
}

/// Cooperative `tasks/cancel` for a block-and-poll `poll_task`: if the poll future
/// is dropped while still polling (a `/stop`) or hits its deadline, tell the server
/// to abandon the task. Fire-and-forget — the response carries no `pending` entry,
/// so the read-loop simply discards it. Disarmed once the task reaches a terminal
/// state (or fails), where cancelling is pointless.
struct TaskCancelOnDrop {
    request_id: u64,
    task_id:    String,
    stdin:      Arc<Mutex<ChildStdin>>,
    name:       String,
    armed:      bool,
}

impl TaskCancelOnDrop {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TaskCancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (request_id, task_id, stdin, name) =
            (self.request_id, self.task_id.clone(), Arc::clone(&self.stdin), self.name.clone());
        tokio::spawn(async move {
            debug!(target: "mcp_client", "[{name}] tasks/cancel for task {task_id}");
            write_json_line(&stdin, &crate::tasks_cancel_request(request_id, &task_id)).await;
        });
    }
}

pub struct McpServer {
    name:    String,
    stdin:   Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    tools:   Vec<McpTool>,
    /// Number of in-flight server→client elicitations awaiting a user reply.
    /// While > 0, an in-flight `tools/call` on this server must not time out
    /// (the user may still be typing a password into the Inbox).
    pending_elicitations: Arc<AtomicUsize>,
    /// Capabilities the server advertised in its `InitializeResult`. Captured so a
    /// future Tasks polling loop can gate on `tasks` support; unused for now.
    server_capabilities: Value,
    /// Cleared by the read-loop the moment the child process is gone, so the
    /// manager can tell "this handle is dead" from "this call failed". Shared with
    /// that task, which is the only writer.
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl McpServer {
    pub async fn start(
        cfg: &McpServerConfig,
        notification_tx: Option<mpsc::UnboundedSender<McpNotification>>,
        log_tx: Option<McpLogTx>,
        elicitation_handler: Option<Arc<dyn ElicitationHandler>>,
    ) -> Result<Self> {
        let command = cfg.command.as_deref()
            .ok_or_else(|| anyhow::anyhow!("stdio server '{}' requires 'command'", cfg.name))?;

        let mut cmd = match &cfg.launch_in {
            // Host transport: spawn the command directly.
            None => {
                let mut c = Command::new(command);
                if let Some(args) = &cfg.args {
                    c.args(args);
                }
                if let Some(env_map) = &cfg.env {
                    for (k, v) in env_map {
                        c.env(k, interpolate_env(v));
                    }
                }
                c
            }
            // Container transport (per-user connectors, blueprint §7): run the
            // command INSIDE the user's container via `docker exec -i`. stdin/
            // stdout/stderr are proxied transparently, so the JSON-RPC read-loop,
            // the stderr drain and elicitation write-back all work unchanged. Env
            // is passed with `-e K=V` so it lands inside the container, not on the
            // `docker` client. Workdir defaults to the image WORKDIR (`/root`, the
            // bind-mounted home), so no `-w` coupling to skald-core's path layout.
            Some(container) => {
                let mut c = Command::new("docker");
                c.arg("exec").arg("-i");
                if let Some(env_map) = &cfg.env {
                    for (k, v) in env_map {
                        c.arg("-e").arg(format!("{k}={}", interpolate_env(v)));
                    }
                }
                c.arg(container).arg(command);
                if let Some(args) = &cfg.args {
                    c.args(args);
                }
                c
            }
        };
        cmd.stdin(Stdio::piped())
           .stdout(Stdio::piped())
           // Capture the child's stderr instead of inheriting it: many MCP
           // servers (e.g. FastMCP) print a startup banner, deprecation
           // warnings and INFO logs there, which would otherwise spill raw
           // onto our console. We drain it into tracing at `debug` level so it
           // stays quiet by default but is still available for diagnostics.
           .stderr(Stdio::piped())
           .kill_on_drop(true);

        // Detach the child into its own process group so that a terminal
        // Ctrl+C (SIGINT delivered to the whole foreground process group)
        // does not reach it directly. Otherwise Python-based MCP servers
        // catch the SIGINT and dump a KeyboardInterrupt traceback to stderr.
        // Cleanup still happens via `kill_on_drop`: when the app shuts down
        // and the reader task is dropped, the child receives SIGKILL silently.
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn '{}': {e}", cfg.name))?;

        let stdin = child.stdin.take()
            .ok_or_else(|| anyhow::anyhow!("could not capture stdin for '{}'", cfg.name))?;
        // Wrap stdin early so both the struct and the read-loop (which writes
        // elicitation replies back to the server) can share the same handle.
        let stdin = Arc::new(Mutex::new(stdin));
        let stdout = child.stdout.take()
            .ok_or_else(|| anyhow::anyhow!("could not capture stdout for '{}'", cfg.name))?;

        // Drain the child's stderr into tracing at `debug` (so banners/warnings
        // don't pollute our console at the default level) *and* forward each line
        // to the host's per-server log file via `log_tx`. Per the MCP 2026 draft,
        // `stderr` is the primary place servers should log, so this is the main
        // diagnostic source for stdio transports.
        if let Some(stderr) = child.stderr.take() {
            let server_name_err = cfg.name.clone();
            let log_tx_err      = log_tx.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty() {
                        debug!(target: "mcp_client", "[{server_name_err}] {line}");
                        if let Some(tx) = &log_tx_err {
                            let _ = tx.send(McpLogLine::stderr(server_name_err.clone(), &line));
                        }
                    }
                }
            });
        }

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let pending_elicitations = Arc::new(AtomicUsize::new(0));

        let alive                    = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let alive_bg                 = Arc::clone(&alive);
        let pending_bg               = pending.clone();
        let server_name_bg           = cfg.name.clone();
        let notification_tx_bg       = notification_tx;
        let log_tx_bg                = log_tx;
        let stdin_bg                 = Arc::clone(&stdin);
        let elicitation_handler_bg   = elicitation_handler;
        let pending_elicitations_bg  = Arc::clone(&pending_elicitations);
        tokio::spawn(async move {
            let mut child = child;
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) if !line.trim().is_empty() => {
                        if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                            let has_method = msg.get("method").is_some();
                            let has_id     = msg.get("id").map(|v| !v.is_null()).unwrap_or(false);
                            if has_method && has_id {
                                // Server → client request (e.g. elicitation/create):
                                // has *both* `method` and `id`, so it must be checked
                                // before the response branch below.
                                handle_server_request(
                                    &server_name_bg, msg, &stdin_bg,
                                    &elicitation_handler_bg, &pending_elicitations_bg,
                                );
                            } else if let Some(id) = msg["id"].as_u64() {
                                if let Some(tx) = pending_bg.lock().await.remove(&id) {
                                    let _ = tx.send(msg);
                                }
                            } else if has_method {
                                // `notifications/message` is the MCP logging utility
                                // (deprecated 2026-07-28): route it to the per-server
                                // log file, not to the notification queue that feeds
                                // event triage — otherwise log records masquerade as business
                                // events. Every other notification (e.g. the custom
                                // `event/*` methods) flows on to `notification_tx`.
                                if msg.get("method").and_then(Value::as_str) == Some("notifications/message") {
                                    if let Some(tx) = &log_tx_bg {
                                        let params = &msg["params"];
                                        let _ = tx.send(McpLogLine::from_message(server_name_bg.clone(), params));
                                    }
                                } else if let Some(tx) = &notification_tx_bg {
                                    let _ = tx.send((server_name_bg.clone(), msg));
                                }
                            }
                        }
                    }
                    Ok(Some(_)) => {}
                    _ => break,
                }
            }
            let exit_info = match child.wait().await {
                Ok(status) if !status.success() => format!(
                    "process exited with {}",
                    status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
                ),
                _ => "process exited unexpectedly".into(),
            };
            // Publish the death *before* failing the pending calls: a caller woken
            // by the error below must find `is_alive() == false`, or it would
            // conclude the call failed on a healthy server and not restart it.
            alive_bg.store(false, Ordering::SeqCst);
            let error_msg = format!("MCP '{}' disconnected: {exit_info}", server_name_bg);
            if let Some(tx) = &log_tx_bg {
                let _ = tx.send(McpLogLine::lifecycle(server_name_bg.clone(), format!("disconnected: {exit_info}")));
            }
            for (_, tx) in pending_bg.lock().await.drain() {
                let _ = tx.send(json!({
                    "jsonrpc": "2.0",
                    "error": { "code": -32000, "message": error_msg }
                }));
            }
        });

        let server = McpServer {
            name:    cfg.name.clone(),
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            tools:   Vec::new(),
            pending_elicitations,
            server_capabilities: json!({}),
            alive,
        };

        let init = server.request("initialize", json!({
            // Declare the elicitation capability (form mode) so servers know they
            // may request input mid-call.
            "protocolVersion": crate::PROTOCOL_VERSION,
            "capabilities":    {
                "elicitation": {},
                // Experimental Tasks marker: we *recognise* a CreateTaskResult
                // (see McpCallResult::Task) but don't poll yet, so we deliberately
                // avoid claiming the full `tasks` capability — that could make a
                // server defer results we can't retrieve. Kept under `experimental`
                // until the polling loop lands.
                "experimental": { "tasks": {} },
            },
            "clientInfo":      { "name": "skald", "version": env!("CARGO_PKG_VERSION") },
        })).await?;
        // Tolerate a server that negotiates a different (older) version — warn but
        // keep going rather than disconnecting.
        if let Some(v) = init["protocolVersion"].as_str() {
            if v != crate::PROTOCOL_VERSION {
                warn!("MCP '{}': server negotiated protocol {v} (we requested {}); proceeding",
                    server.name, crate::PROTOCOL_VERSION);
            }
        }
        // Capture the server's advertised capabilities for a future Tasks poller.
        let server_capabilities = init.get("capabilities").cloned().unwrap_or_else(|| json!({}));

        server.notify("notifications/initialized", json!({})).await?;

        // Follow `nextCursor` across pages so servers with large tool lists aren't
        // silently truncated; capped at `MAX_TOOL_PAGES` against a stuck cursor.
        let mut tools: Vec<McpTool> = Vec::new();
        let mut cursor: Option<String> = None;
        for page_n in 0..crate::MAX_TOOL_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None    => json!({}),
            };
            let page = server.request("tools/list", params).await?;
            if let Some(arr) = page["tools"].as_array() {
                tools.extend(arr.iter().map(|t| McpTool::from_json(&cfg.name, t)));
            }
            cursor = page["nextCursor"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
            if cursor.is_none() {
                break;
            }
            if page_n + 1 == crate::MAX_TOOL_PAGES {
                warn!("MCP '{}': tools/list hit {}-page cap; some tools may be omitted",
                    server.name, crate::MAX_TOOL_PAGES);
            }
        }

        Ok(McpServer { tools, server_capabilities, ..server })
    }

    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// Capabilities the server advertised at `initialize`. Exposed for a future
    /// Tasks polling loop to gate on `tasks` support.
    pub fn server_capabilities(&self) -> &Value {
        &self.server_capabilities
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> Result<McpCallResult> {
        let mut params = json!({ "name": name, "arguments": args });
        if self.wants_task(name) {
            // Opt into deferred execution for a task-capable tool (experimental
            // Tasks). Per spec, adding the `task` field to the request is the
            // opt-in for `tools/call` (the client is the requestor).
            params["task"] = json!({});
        }
        let result = self.request("tools/call", params).await?;

        if result["isError"].as_bool().unwrap_or(false) {
            anyhow::bail!("MCP tool error: {}", extract_text(&result));
        }
        match crate::extract_call_result(&result) {
            McpCallResult::Task(task) => self.poll_task(task).await,
            other => Ok(other),
        }
    }

    /// True when tool `name` advertises `execution.taskSupport` as `required`/
    /// `optional`, so we opt into deferred (Task) execution.
    fn wants_task(&self, name: &str) -> bool {
        self.tools.iter()
            .find(|t| t.name == name)
            .and_then(|t| t.task_support.as_deref())
            .is_some_and(|s| s == "required" || s == "optional")
    }

    /// Drives a deferred Task to completion (experimental Tasks, block-and-poll):
    /// polls `tasks/get` until a terminal status, then fetches the real result via
    /// `tasks/result`. A [`TaskCancelOnDrop`] guard sends `tasks/cancel` if this
    /// future is dropped (a `/stop`) or the deadline is hit. Each poll request is a
    /// normal short `request()` (subject to the 120s timeout); the *overall* wait is
    /// bounded only by the task's `ttl` — so long tasks no longer hit that wall.
    async fn poll_task(&self, task: crate::CreateTaskResult) -> Result<McpCallResult> {
        let task_id  = task.task_id.as_str();
        let cancel_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut guard = TaskCancelOnDrop {
            request_id: cancel_id,
            task_id:    task.task_id.clone(),
            stdin:      Arc::clone(&self.stdin),
            name:       self.name.clone(),
            armed:      true,
        };

        let deadline     = crate::poll_deadline(task.ttl_ms);
        let mut interval = task.poll_interval_ms;

        loop {
            tokio::time::sleep(crate::clamp_poll_interval(interval)).await;
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("MCP '{}' task '{task_id}' exceeded max wait", self.name);
            }
            let get = self.request("tasks/get", json!({ "taskId": task_id })).await?;
            let Some(state) = crate::CreateTaskResult::parse(&get) else {
                anyhow::bail!("MCP '{}' task '{task_id}': malformed tasks/get response", self.name);
            };
            interval = state.poll_interval_ms.or(interval);
            match state.status {
                crate::TaskStatus::Working    => continue,
                crate::TaskStatus::Completed  => break,
                crate::TaskStatus::Failed => {
                    guard.disarm();
                    anyhow::bail!("MCP '{}' task '{task_id}' failed: {}", self.name, extract_text(&get));
                }
                crate::TaskStatus::Cancelled => {
                    guard.disarm();
                    anyhow::bail!("MCP '{}' task '{task_id}' was cancelled by the server", self.name);
                }
                // Still alive but blocked on input we can't supply mid-task: abandon
                // it (guard stays armed → tasks/cancel). See follow-up.
                crate::TaskStatus::InputRequired =>
                    anyhow::bail!("MCP '{}' task '{task_id}' requires input mid-task, which isn't supported yet", self.name),
            }
        }

        // Task is terminal (completed) — nothing left to cancel.
        guard.disarm();
        let result = self.request("tasks/result", json!({ "taskId": task_id })).await?;

        if result["isError"].as_bool().unwrap_or(false) {
            anyhow::bail!("MCP tool error: {}", extract_text(&result));
        }
        Ok(crate::extract_call_result(&result))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        // Arm a cancellation guard for cancellable operations only (`tools/call`):
        // if this future is dropped by a `/stop` or times out before the server
        // replies, the guard notifies the server. Disarmed on a real reply.
        let mut cancel_guard = (method == "tools/call").then(|| CancelOnDrop {
            id,
            stdin:   Arc::clone(&self.stdin),
            pending: Arc::clone(&self.pending),
            name:    self.name.clone(),
            reason:  "cancelled by client",
            armed:   true,
        });

        let msg = json!({
            "jsonrpc": "2.0",
            "id":      id,
            "method":  method,
            "params":  params,
        });
        let mut line = serde_json::to_string(&msg)?;
        line.push('\n');
        self.stdin.lock().await.write_all(line.as_bytes()).await?;

        // Wait for the response, but re-arm the timeout while an elicitation is
        // in flight on this server: the user may still be typing a secret into
        // the Inbox, and the server won't answer `tools/call` until then.
        tokio::pin!(rx);
        let response = loop {
            tokio::select! {
                res = &mut rx => match res {
                    Ok(v) => break v,
                    Err(_) => {
                        // Server disconnected: nothing to cancel.
                        if let Some(g) = cancel_guard.as_mut() { g.disarm(); }
                        anyhow::bail!("MCP '{}' disconnected", self.name);
                    }
                },
                _ = tokio::time::sleep(Duration::from_secs(CALL_TIMEOUT_SECS)) => {
                    if self.pending_elicitations.load(Ordering::SeqCst) == 0 {
                        // Leave the guard armed so it fires `notifications/cancelled`;
                        // tag the reason as a timeout.
                        if let Some(g) = cancel_guard.as_mut() { g.reason = "timeout"; }
                        anyhow::bail!("MCP '{}' timed out on '{method}'", self.name);
                    }
                    // Elicitation pending: keep waiting for the user.
                }
            }
        };

        // Server replied (even with an error): the request completed — disarm.
        if let Some(g) = cancel_guard.as_mut() { g.disarm(); }

        if let Some(error) = response.get("error") {
            anyhow::bail!("MCP '{}' protocol error: {error}", self.name);
        }
        Ok(response["result"].clone())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method":  method,
            "params":  params,
        });
        let mut line = serde_json::to_string(&msg)?;
        line.push('\n');
        self.stdin.lock().await.write_all(line.as_bytes()).await?;
        Ok(())
    }
}

#[async_trait]
impl McpServerClient for McpServer {
    fn tools(&self) -> &[McpTool] { self.tools() }
    async fn call_tool(&self, name: &str, args: Value) -> Result<McpCallResult> { self.call_tool(name, args).await }
    fn is_alive(&self) -> bool { self.alive.load(Ordering::SeqCst) }
}
