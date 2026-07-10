use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tracing::{debug, warn};

use crate::{McpCallResult, McpServerClient, McpTool, extract_text, interpolate_env};
use crate::config::McpServerConfig;

const CALL_TIMEOUT_SECS: u64 = 120;

/// Best-effort cancellation for an in-flight HTTP `tools/call`: if dropped while
/// armed (a `/stop` drops the request future, or the request timed out), it POSTs
/// `notifications/cancelled` so the server can stop. Correlation is weaker than on
/// stdio — the server must map `requestId` to the abandoned POST, which not every
/// server does — hence best-effort. Disarmed once the server responds (or when a
/// non-timeout send error proves the server never received the request).
struct HttpCancelOnDrop {
    id:      u64,
    client:  reqwest::Client,
    url:     String,
    headers: HeaderMap,
    name:    String,
    reason:  &'static str,
    armed:   bool,
}

impl HttpCancelOnDrop {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for HttpCancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (id, client, url, headers, name, reason) =
            (self.id, self.client.clone(), self.url.clone(), self.headers.clone(), self.name.clone(), self.reason);
        tokio::spawn(async move {
            debug!("MCP http '{name}': notifications/cancelled for request {id} ({reason})");
            let _ = client.post(&url).headers(headers)
                .json(&crate::cancelled_notification(id, reason))
                .send().await;
        });
    }
}

/// Cooperative `tasks/cancel` for a block-and-poll `poll_task` over HTTP: if the
/// poll future is dropped while still polling (a `/stop`) or hits its deadline,
/// POST `tasks/cancel` best-effort. Disarmed once the task reaches a terminal state.
struct HttpTaskCancelOnDrop {
    request_id: u64,
    task_id:    String,
    client:     reqwest::Client,
    url:        String,
    headers:    HeaderMap,
    name:       String,
    armed:      bool,
}

impl HttpTaskCancelOnDrop {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for HttpTaskCancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let (request_id, task_id, client, url, headers, name) =
            (self.request_id, self.task_id.clone(), self.client.clone(), self.url.clone(), self.headers.clone(), self.name.clone());
        tokio::spawn(async move {
            debug!("MCP http '{name}': tasks/cancel for task {task_id}");
            let _ = client.post(&url).headers(headers)
                .json(&crate::tasks_cancel_request(request_id, &task_id))
                .send().await;
        });
    }
}

pub struct McpHttpServer {
    name:       String,
    url:        String,
    client:     reqwest::Client,
    headers:    HeaderMap,
    /// Set after the `initialize` response — required by stateful servers like Tavily.
    session_id: Mutex<Option<String>>,
    /// Protocol version negotiated in the `initialize` response (falls back to
    /// [`crate::PROTOCOL_VERSION`]). Once set, echoed in the `MCP-Protocol-Version`
    /// header on every post-initialize request, per the Streamable HTTP spec.
    protocol_version: Mutex<Option<String>>,
    next_id:    AtomicU64,
    tools:      Vec<McpTool>,
    /// Capabilities the server advertised in its `InitializeResult`. Captured so a
    /// future Tasks polling loop can gate on `tasks` support; unused for now.
    server_capabilities: Value,
}

impl McpHttpServer {
    pub async fn start(cfg: &McpServerConfig) -> Result<Self> {
        let url = cfg.url.as_deref()
            .ok_or_else(|| anyhow::anyhow!("http server '{}' requires 'url'", cfg.name))?
            .trim_end_matches('/')
            .to_string();

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("application/json, text/event-stream"));

        if let Some(key) = &cfg.api_key {
            let val = interpolate_env(key);
            let bearer = format!("Bearer {val}");
            headers.insert(AUTHORIZATION, bearer.parse()
                .map_err(|_| anyhow::anyhow!("invalid api_key for '{}'", cfg.name))?);
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(CALL_TIMEOUT_SECS))
            .build()?;

        let server = McpHttpServer {
            name:       cfg.name.clone(),
            url,
            client,
            headers,
            session_id:       Mutex::new(None),
            protocol_version: Mutex::new(None),
            next_id:    AtomicU64::new(1),
            tools:      Vec::new(),
            server_capabilities: json!({}),
        };

        let init = server.request("initialize", json!({
            // The HTTP transport doesn't service the ElicitationHandler (stdio-only),
            // so it must NOT advertise the elicitation capability.
            "protocolVersion": crate::PROTOCOL_VERSION,
            // Experimental Tasks marker only (recognise-but-don't-poll); see the
            // stdio transport for the rationale behind keeping it under `experimental`.
            "capabilities":    { "experimental": { "tasks": {} } },
            "clientInfo":      { "name": "skald", "version": env!("CARGO_PKG_VERSION") },
        })).await?;
        // Capture the negotiated version (fall back to our own) so post-initialize
        // requests can echo it in the MCP-Protocol-Version header; tolerate a
        // downgrade with a warning rather than disconnecting.
        let negotiated = init["protocolVersion"].as_str().unwrap_or(crate::PROTOCOL_VERSION);
        if negotiated != crate::PROTOCOL_VERSION {
            warn!("MCP http '{}': server negotiated protocol {negotiated} (we requested {}); proceeding",
                server.name, crate::PROTOCOL_VERSION);
        }
        *server.protocol_version.lock().unwrap() = Some(negotiated.to_string());
        // Capture the server's advertised capabilities for a future Tasks poller.
        let server_capabilities = init.get("capabilities").cloned().unwrap_or_else(|| json!({}));

        if let Err(e) = server.notify("notifications/initialized", json!({})).await {
            warn!("MCP http '{}': initialized notification failed (ignoring): {e}", server.name);
        }

        // Follow `nextCursor` across pages so large tool lists aren't silently
        // truncated; capped at `MAX_TOOL_PAGES` against a stuck cursor.
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
                warn!("MCP http '{}': tools/list hit {}-page cap; some tools may be omitted",
                    server.name, crate::MAX_TOOL_PAGES);
            }
        }

        Ok(McpHttpServer { tools, server_capabilities, ..server })
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
            // Opt into deferred execution for a task-capable tool (experimental Tasks).
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
    /// `tasks/result`. A [`HttpTaskCancelOnDrop`] guard POSTs `tasks/cancel` if this
    /// future is dropped (a `/stop`) or the deadline is hit. The overall wait is
    /// bounded only by the task's `ttl`, so long tasks no longer hit the 120s wall.
    async fn poll_task(&self, task: crate::CreateTaskResult) -> Result<McpCallResult> {
        let task_id   = task.task_id.as_str();
        let cancel_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut guard = HttpTaskCancelOnDrop {
            request_id: cancel_id,
            task_id:    task.task_id.clone(),
            client:     self.client.clone(),
            url:        self.url.clone(),
            headers:    self.request_headers(),
            name:       self.name.clone(),
            armed:      true,
        };

        let deadline     = crate::poll_deadline(task.ttl_ms);
        let mut interval = task.poll_interval_ms;

        loop {
            tokio::time::sleep(crate::clamp_poll_interval(interval)).await;
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("MCP http '{}' task '{task_id}' exceeded max wait", self.name);
            }
            let get = self.request("tasks/get", json!({ "taskId": task_id })).await?;
            let Some(state) = crate::CreateTaskResult::parse(&get) else {
                anyhow::bail!("MCP http '{}' task '{task_id}': malformed tasks/get response", self.name);
            };
            interval = state.poll_interval_ms.or(interval);
            match state.status {
                crate::TaskStatus::Working    => continue,
                crate::TaskStatus::Completed  => break,
                crate::TaskStatus::Failed => {
                    guard.disarm();
                    anyhow::bail!("MCP http '{}' task '{task_id}' failed: {}", self.name, extract_text(&get));
                }
                crate::TaskStatus::Cancelled => {
                    guard.disarm();
                    anyhow::bail!("MCP http '{}' task '{task_id}' was cancelled by the server", self.name);
                }
                crate::TaskStatus::InputRequired =>
                    anyhow::bail!("MCP http '{}' task '{task_id}' requires input mid-task, which isn't supported yet", self.name),
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

    /// Builds per-request headers: the static base plus the captured
    /// `Mcp-Session-Id` and `MCP-Protocol-Version`. Both are set only after the
    /// `initialize` response, so they're naturally absent on the initialize call
    /// itself (the spec scopes the version header to post-initialize requests).
    fn request_headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        if let Some(sid) = self.session_id.lock().unwrap().as_deref() {
            if let Ok(val) = HeaderValue::from_str(sid) {
                headers.insert("mcp-session-id", val);
            }
        }
        if let Some(ver) = self.protocol_version.lock().unwrap().as_deref() {
            if let Ok(val) = HeaderValue::from_str(ver) {
                headers.insert("mcp-protocol-version", val);
            }
        }
        headers
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = json!({
            "jsonrpc": "2.0",
            "id":      id,
            "method":  method,
            "params":  params,
        });

        let req_headers = self.request_headers();

        // Arm a best-effort cancellation guard for cancellable operations only
        // (`tools/call`): a `/stop` that drops this future, or a request timeout,
        // then POSTs `notifications/cancelled`. Disarmed once the server responds.
        let mut cancel_guard = (method == "tools/call").then(|| HttpCancelOnDrop {
            id,
            client:  self.client.clone(),
            url:     self.url.clone(),
            headers: req_headers.clone(),
            name:    self.name.clone(),
            reason:  "cancelled by client",
            armed:   true,
        });

        let resp = match self.client
            .post(&self.url)
            .headers(req_headers)
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // A timeout may leave the server still working → cancel it. Any
                // other send failure means the server never got the request, so
                // there is nothing to cancel.
                if let Some(g) = cancel_guard.as_mut() {
                    if e.is_timeout() { g.reason = "timeout"; } else { g.disarm(); }
                }
                anyhow::bail!("MCP http '{}' request failed: {e}", self.name);
            }
        };

        // The server responded — the request completed on its side; disarm.
        if let Some(g) = cancel_guard.as_mut() { g.disarm(); }

        if let Some(sid) = resp.headers().get("mcp-session-id") {
            if let Ok(sid_str) = sid.to_str() {
                debug!("MCP http '{}': captured session id", self.name);
                *self.session_id.lock().unwrap() = Some(sid_str.to_string());
            }
        }

        let status = resp.status();
        let content_type = resp.headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let msg: Value = if content_type.contains("text/event-stream") {
            parse_sse_response(resp).await
                .map_err(|e| anyhow::anyhow!("MCP http '{}' SSE parse error: {e}", self.name))?
        } else {
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("MCP http '{}' HTTP {status}: {body}", self.name);
            }
            resp.json::<Value>().await
                .map_err(|e| anyhow::anyhow!("MCP http '{}' JSON decode error: {e}", self.name))?
        };

        if let Some(error) = msg.get("error") {
            anyhow::bail!("MCP http '{}' protocol error: {error}", self.name);
        }
        Ok(msg["result"].clone())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "method":  method,
            "params":  params,
        });

        let req_headers = self.request_headers();

        self.client
            .post(&self.url)
            .headers(req_headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("MCP http notify failed: {e}"))?;
        Ok(())
    }
}

#[async_trait]
impl McpServerClient for McpHttpServer {
    fn tools(&self) -> &[McpTool] { self.tools() }
    async fn call_tool(&self, name: &str, args: Value) -> Result<McpCallResult> { self.call_tool(name, args).await }
}

async fn parse_sse_response(resp: reqwest::Response) -> Result<Value> {
    let text = resp.text().await?;
    for line in text.lines() {
        let data = match line.strip_prefix("data:") {
            Some(d) => d.trim(),
            None    => continue,
        };
        if data == "[DONE]" { break; }
        if let Ok(msg) = serde_json::from_str::<Value>(data) {
            if msg.get("result").is_some() || msg.get("error").is_some() {
                return Ok(msg);
            }
        }
    }
    anyhow::bail!("no JSON-RPC result found in SSE response")
}
