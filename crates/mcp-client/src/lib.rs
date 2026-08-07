pub mod config;
pub mod http_server;
pub mod log;
pub mod server;

use async_trait::async_trait;
use serde_json::{Value, json};

pub use config::{McpServerConfig, McpTransport};
pub use log::{McpLogLine, McpLogTx};
pub use server::{
    ElicitationAction, ElicitationHandler, ElicitationReply, ElicitationRequest, McpNotification,
};

/// MCP protocol version this client advertises in `initialize` and (over HTTP)
/// echoes in the `MCP-Protocol-Version` header on every post-initialize request.
/// Shared by both transports so they can never drift apart. The host tolerates a
/// server negotiating a different (older) version — see the HTTP transport, which
/// captures the server's reply and warns rather than disconnecting.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Safety cap on `tools/list` pagination: stop following `nextCursor` after this
/// many pages so a buggy or hostile server that never clears the cursor can't
/// loop the client forever.
pub(crate) const MAX_TOOL_PAGES: usize = 50;

/// Builds a `notifications/cancelled` message for an in-flight request. Shared by
/// both transports (like [`PROTOCOL_VERSION`]) so the wire shape can't drift. Per
/// the MCP spec the client MUST NOT cancel the `initialize` request; callers only
/// arm this for cancellable operations (`tools/call`).
pub(crate) fn cancelled_notification(request_id: u64, reason: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method":  "notifications/cancelled",
        "params":  { "requestId": request_id, "reason": reason },
    })
}

/// Builds a `tasks/cancel` request for a task the client is abandoning
/// (experimental Tasks). Sent fire-and-forget from the poll cancel-guard — the
/// response is ignored — so it carries a pre-allocated `request_id`. Shared by both
/// transports.
pub(crate) fn tasks_cancel_request(request_id: u64, task_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id":      request_id,
        "method":  "tasks/cancel",
        "params":  { "taskId": task_id },
    })
}

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct McpTool {
    pub server_name:   String,
    pub name:          String,
    pub description:   String,
    pub input_schema:  Value,
    /// Optional human-readable title (MCP 2025-06-18+).
    pub title:         Option<String>,
    /// Optional JSON Schema describing the tool's structured output
    /// (`outputSchema`, MCP 2025-06-18+). Captured for future validation and to
    /// know a tool can return `structuredContent`.
    pub output_schema: Option<Value>,
    /// Optional tool annotations (`readOnlyHint`, `destructiveHint`, …), treated
    /// as untrusted hints per the spec.
    pub annotations:   Option<Value>,
    /// Optional per-tool Tasks negotiation (`execution.taskSupport`:
    /// `required`/`optional`/`forbidden`, MCP 2025-11-25 experimental). Captured
    /// as an untrusted hint so a future polling implementation knows which tools
    /// may return a [`CreateTaskResult`]. See [`McpCallResult::Task`].
    pub task_support:  Option<String>,
}

impl McpTool {
    /// Builds an [`McpTool`] from one entry of a `tools/list` `tools[]` array.
    /// Shared by both transports so the field mapping (incl. the 2025-06-18+
    /// `title`/`outputSchema`/`annotations`) stays in one place.
    pub fn from_json(server_name: &str, t: &Value) -> McpTool {
        McpTool {
            server_name:   server_name.to_string(),
            name:          t["name"].as_str().unwrap_or("").to_string(),
            description:   t["description"].as_str().unwrap_or("").to_string(),
            input_schema:  t.get("inputSchema").cloned().unwrap_or_else(|| json!({
                "type": "object", "properties": {},
            })),
            title:         t.get("title").and_then(Value::as_str).map(str::to_string),
            output_schema: t.get("outputSchema").cloned(),
            annotations:   t.get("annotations").cloned(),
            task_support:  t.get("execution")
                .and_then(|e| e.get("taskSupport"))
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    }

    pub fn tool_id(&self) -> String {
        format!("mcp__{}__{}", self.server_name, self.name)
    }

    pub fn to_openai_definition(&self) -> Value {
        let params = if self.input_schema.is_object() {
            self.input_schema.clone()
        } else {
            json!({ "type": "object", "properties": {} })
        };
        json!({
            "type": "function",
            "function": {
                "name":        self.tool_id(),
                "description": format!("[{}] {}", self.server_name, self.description),
                "parameters":  params,
            }
        })
    }
}

/// Status of a configured MCP server.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpServerStatus {
    Running { tools: Vec<String> },
    Error   { message: String },
    Disabled,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServerInfo {
    pub name:      String,
    pub transport: String,
    pub description:   Option<String>,
    pub friendly_name: Option<String>,
    #[serde(flatten)]
    pub status:    McpServerStatus,
}

// ── Tool-result media ──────────────────────────────────────────────────────────

/// Kind of a non-text content block carried in an MCP `tools/call` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpMediaKind {
    Image,
    Audio,
    /// Embedded or linked resource (any other binary/file payload).
    Resource,
}

/// Payload of one media content block: either decoded inline bytes (from an
/// `image`/`audio`/embedded-`resource` base64 field) or a link to a remote
/// resource (`resource_link`), which the crate does not download.
#[derive(Debug, Clone)]
pub enum McpMediaData {
    /// Decoded bytes plus the server-declared MIME type.
    Inline { bytes: Vec<u8>, mime: String },
    /// A `resource_link` URI, passed through untouched (no fetch here).
    Link { uri: String, mime: Option<String> },
}

/// One non-text content block extracted from a `tools/call` result. The crate
/// stays a generic transport: it decodes base64 to bytes but never touches the
/// disk — persistence is the host's job (`McpManager`).
#[derive(Debug, Clone)]
pub struct McpMedia {
    pub kind: McpMediaKind,
    pub data: McpMediaData,
}

// ── Tasks (MCP 2025-11-25, experimental) ────────────────────────────────────────

/// Lifecycle state of a Task (`CreateTaskResult.status`). `Working` transitions to
/// `Completed`/`Failed`/`Cancelled`; `InputRequired` means the receiver needs more
/// input (e.g. an elicitation) before it can continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Working,
    Completed,
    Failed,
    Cancelled,
    InputRequired,
}

impl TaskStatus {
    /// A terminal status: the task will not change further, so polling stops.
    /// `InputRequired` is **not** terminal (the task resumes once input is given),
    /// but the current block-and-poll client can't fulfil input mid-task, so it
    /// treats it as an error rather than looping forever — see `poll_task`.
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled)
    }
}

/// Clamps a server-suggested `pollInterval` (ms) into a sane range: default 1s when
/// absent, floored at 500ms (don't hammer the server) and capped at 30s (stay
/// responsive). Shared by both transports so their poll cadence can't drift.
pub(crate) fn clamp_poll_interval(ms: Option<u64>) -> std::time::Duration {
    std::time::Duration::from_millis(ms.unwrap_or(1000).clamp(500, 30_000))
}

/// Deadline after which the block-and-poll loop gives up on a task and cancels it:
/// the server's `ttl` when given, otherwise a generous 1-hour cap so a stuck task
/// can't pin a session forever.
pub(crate) fn poll_deadline(ttl_ms: Option<u64>) -> std::time::Instant {
    let ttl = std::time::Duration::from_millis(ttl_ms.unwrap_or(3_600_000));
    std::time::Instant::now() + ttl
}

/// A durable task handle returned by a receiver when a request is executed as a
/// deferred, pollable operation instead of blocking (MCP 2025-11-25 *experimental*
/// Tasks). The client polls `tasks/get` until a terminal status, then fetches the
/// real result via `tasks/result` (see `poll_task` on each transport). Also used to
/// parse each `tasks/get` response (same shape).
#[derive(Debug, Clone)]
pub struct CreateTaskResult {
    pub task_id:          String,
    pub status:           TaskStatus,
    /// Suggested delay between `tasks/get` polls, in milliseconds.
    pub poll_interval_ms: Option<u64>,
    /// Time-to-live of the task handle, in milliseconds.
    pub ttl_ms:           Option<u64>,
}

impl CreateTaskResult {
    /// Parses a `tools/call` result that carries a task handle. Returns `None` if
    /// `v` is not a task-shaped object (no `taskId`). The handle may live at the
    /// top level or under a `task` field, depending on the server.
    pub(crate) fn parse(v: &Value) -> Option<CreateTaskResult> {
        let obj = v.get("task").filter(|t| t.is_object()).unwrap_or(v);
        let task_id = obj.get("taskId").and_then(Value::as_str)?.to_string();
        let status = obj.get("status")
            .and_then(|s| serde_json::from_value::<TaskStatus>(s.clone()).ok())
            .unwrap_or(TaskStatus::Working);
        Some(CreateTaskResult {
            task_id,
            status,
            poll_interval_ms: obj.get("pollInterval").and_then(Value::as_u64),
            ttl_ms:           obj.get("ttl").and_then(Value::as_u64),
        })
    }
}

// ── Transport trait ───────────────────────────────────────────────────────────

/// Typed result of an MCP `tools/call`. Mirrors the host's tool-result shape
/// without coupling this crate to it; the host (`McpManager`) maps it. Per the
/// MCP spec, `structuredContent` is canonical when present (servers SHOULD also
/// mirror it in a `TextContent` block for backwards compatibility); we prefer it
/// and fall back to the joined `text` items, which fixes the silent empty-result
/// case for servers that omit the text mirror. Non-text content blocks
/// (`image`/`audio`/`resource`/`resource_link`) are surfaced via [`McpCallResult::Media`]
/// so the host can persist them instead of dropping them.
#[derive(Debug, Clone)]
pub enum McpCallResult {
    /// Joined `text` content items.
    Text(String),
    /// Canonical structured payload from `structuredContent`.
    Json(Value),
    /// At least one non-text media block was present. `text`/`structured` carry
    /// any accompanying textual/structured payload from the same result.
    Media {
        text:       Option<String>,
        structured: Option<Value>,
        items:      Vec<McpMedia>,
    },
    /// The server deferred the call as a Task (experimental Tasks, 2025-11-25) and
    /// returned a durable handle instead of a result. Skald surfaces the handle;
    /// polling for the real result is a follow-up.
    Task(CreateTaskResult),
}

#[async_trait]
pub trait McpServerClient: Send + Sync {
    fn tools(&self) -> &[McpTool];
    async fn call_tool(&self, name: &str, args: Value) -> anyhow::Result<McpCallResult>;

    /// Whether this connection is still usable.
    ///
    /// A stdio server *is* its child process: once that exits, the handle stays in
    /// the manager's map but every call on it fails with a disconnect error, so
    /// something has to be able to ask. The default is `true` for HTTP/SSE, which
    /// holds no process and no long-lived connection — a dead remote surfaces per
    /// call, and answering `false` here would make the manager "restart" a server
    /// that was never running.
    fn is_alive(&self) -> bool {
        true
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parses `mcp__<server>__<tool>` → `(server, tool)`.
pub fn parse_mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let sep = rest.find("__")?;
    Some((&rest[..sep], &rest[sep + 2..]))
}

/// Extracts text content from an MCP tool result value (the `text` items of
/// `content[]`, plus the inline `text` of embedded text resources). Used for the
/// `isError` path and as the text component of a successful result.
pub(crate) fn extract_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|arr| classify_content(arr).0)
        .unwrap_or_default()
}

/// Decodes a standard-base64 string to bytes, returning `None` on malformed input.
fn decode_b64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

/// Walks `content[]`, returning the joined text and any non-text media blocks
/// (`image`/`audio`/embedded-`resource`/`resource_link`). Base64 payloads are
/// decoded to bytes here; persistence is left to the host.
fn classify_content(content: &[Value]) -> (String, Vec<McpMedia>) {
    let mut texts: Vec<String>  = Vec::new();
    let mut media: Vec<McpMedia> = Vec::new();

    for item in content {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    texts.push(t.to_string());
                }
            }
            Some(kind @ ("image" | "audio")) => {
                let mime = item.get("mimeType").and_then(Value::as_str)
                    .unwrap_or("application/octet-stream").to_string();
                if let Some(bytes) = item.get("data").and_then(Value::as_str).and_then(decode_b64) {
                    let kind = if kind == "image" { McpMediaKind::Image } else { McpMediaKind::Audio };
                    media.push(McpMedia { kind, data: McpMediaData::Inline { bytes, mime } });
                }
            }
            Some("resource") => {
                // Embedded resource: a base64 `blob` (binary) or inline `text`.
                let res  = item.get("resource");
                let mime = res.and_then(|r| r.get("mimeType")).and_then(Value::as_str)
                    .unwrap_or("application/octet-stream").to_string();
                if let Some(bytes) = res.and_then(|r| r.get("blob")).and_then(Value::as_str).and_then(decode_b64) {
                    media.push(McpMedia { kind: McpMediaKind::Resource, data: McpMediaData::Inline { bytes, mime } });
                } else if let Some(t) = res.and_then(|r| r.get("text")).and_then(Value::as_str) {
                    texts.push(t.to_string());
                }
            }
            Some("resource_link") => {
                if let Some(uri) = item.get("uri").and_then(Value::as_str) {
                    let mime = item.get("mimeType").and_then(Value::as_str).map(str::to_string);
                    media.push(McpMedia {
                        kind: McpMediaKind::Resource,
                        data: McpMediaData::Link { uri: uri.to_string(), mime },
                    });
                }
            }
            // Unknown/typeless block: capture a bare `text` field if present.
            _ => {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    texts.push(t.to_string());
                }
            }
        }
    }
    (texts.join("\n"), media)
}

/// Builds the typed result of an MCP `tools/call`. When any non-text media block
/// is present it returns [`McpCallResult::Media`] (so the host can persist the
/// bytes instead of dropping them). Otherwise it preserves the original
/// precedence: `structuredContent` is canonical when present, else the joined
/// `text` items — which also fixes the silent empty-result case for servers that
/// return only `structuredContent` without the recommended text mirror.
pub(crate) fn extract_call_result(result: &Value) -> McpCallResult {
    // A Task handle (deferred execution) has no `content[]`; recognise it first so
    // it isn't mistaken for an empty result.
    if result.get("content").is_none() {
        if let Some(task) = CreateTaskResult::parse(result) {
            return McpCallResult::Task(task);
        }
    }

    let content    = result["content"].as_array().cloned().unwrap_or_default();
    let (text, media) = classify_content(&content);
    let structured = result.get("structuredContent").filter(|v| !v.is_null()).cloned();

    if !media.is_empty() {
        return McpCallResult::Media {
            text:       (!text.is_empty()).then_some(text),
            structured,
            items:      media,
        };
    }
    if let Some(sc) = structured {
        return McpCallResult::Json(sc);
    }
    McpCallResult::Text(text)
}

/// Interpolates `${VAR}` references in a string from the process environment.
pub(crate) fn interpolate_env(s: &str) -> String {
    let mut result = s.to_string();
    loop {
        let Some(start) = result.find("${") else { break };
        let Some(rel_end) = result[start..].find('}') else { break };
        let var_name = result[start + 2..start + rel_end].to_string();
        let value = std::env::var(&var_name).unwrap_or_else(|_| {
            tracing::warn!("MCP env var ${{{var_name}}} not set");
            String::new()
        });
        result = format!("{}{}{}", &result[..start], value, &result[start + rel_end + 1..]);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn image_block_becomes_media_with_decoded_bytes() {
        let png = [0x89u8, b'P', b'N', b'G'];
        let result = json!({
            "content": [
                { "type": "text",  "text": "here is your image" },
                { "type": "image", "data": b64(&png), "mimeType": "image/png" }
            ]
        });
        match extract_call_result(&result) {
            McpCallResult::Media { text, items, structured } => {
                assert_eq!(text.as_deref(), Some("here is your image"));
                assert!(structured.is_none());
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].kind, McpMediaKind::Image);
                match &items[0].data {
                    McpMediaData::Inline { bytes, mime } => {
                        assert_eq!(bytes.as_slice(), &png);
                        assert_eq!(mime, "image/png");
                    }
                    _ => panic!("expected inline media"),
                }
            }
            other => panic!("expected Media, got {other:?}"),
        }
    }

    #[test]
    fn text_only_stays_text() {
        let result = json!({ "content": [ { "type": "text", "text": "plain" } ] });
        match extract_call_result(&result) {
            McpCallResult::Text(t) => assert_eq!(t, "plain"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn structured_without_media_stays_json() {
        let result = json!({
            "content": [ { "type": "text", "text": "mirror" } ],
            "structuredContent": { "ok": true }
        });
        match extract_call_result(&result) {
            McpCallResult::Json(v) => assert_eq!(v, json!({ "ok": true })),
            other => panic!("expected Json, got {other:?}"),
        }
    }

    #[test]
    fn cancelled_notification_has_request_id_and_reason() {
        let msg = cancelled_notification(7, "timeout");
        assert_eq!(msg["jsonrpc"], "2.0");
        assert_eq!(msg["method"], "notifications/cancelled");
        assert!(msg.get("id").is_none(), "notifications MUST NOT carry an id");
        assert_eq!(msg["params"]["requestId"], 7);
        assert_eq!(msg["params"]["reason"], "timeout");
    }

    #[test]
    fn from_json_captures_task_support() {
        let t = json!({
            "name": "gen_video",
            "execution": { "taskSupport": "required" }
        });
        let tool = McpTool::from_json("srv", &t);
        assert_eq!(tool.task_support.as_deref(), Some("required"));

        // Absent execution → None.
        let plain = McpTool::from_json("srv", &json!({ "name": "echo" }));
        assert!(plain.task_support.is_none());
    }

    #[test]
    fn create_task_result_parses_top_level_and_nested() {
        // Top-level handle.
        let top = json!({
            "taskId": "abc-123", "status": "working",
            "pollInterval": 500, "ttl": 60000
        });
        let r = CreateTaskResult::parse(&top).expect("top-level task");
        assert_eq!(r.task_id, "abc-123");
        assert_eq!(r.status, TaskStatus::Working);
        assert_eq!(r.poll_interval_ms, Some(500));
        assert_eq!(r.ttl_ms, Some(60000));

        // Nested under `task`, unknown status → defaults to Working.
        let nested = json!({ "task": { "taskId": "x", "status": "input_required" } });
        let r = CreateTaskResult::parse(&nested).expect("nested task");
        assert_eq!(r.task_id, "x");
        assert_eq!(r.status, TaskStatus::InputRequired);

        // Not task-shaped → None.
        assert!(CreateTaskResult::parse(&json!({ "content": [] })).is_none());
    }

    #[test]
    fn task_status_is_terminal() {
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
        assert!(!TaskStatus::Working.is_terminal());
        assert!(!TaskStatus::InputRequired.is_terminal());
    }

    #[test]
    fn clamp_poll_interval_defaults_and_bounds() {
        use std::time::Duration;
        assert_eq!(clamp_poll_interval(None),        Duration::from_millis(1000)); // default
        assert_eq!(clamp_poll_interval(Some(10)),    Duration::from_millis(500));  // floor
        assert_eq!(clamp_poll_interval(Some(99_999)), Duration::from_millis(30_000)); // cap
        assert_eq!(clamp_poll_interval(Some(2000)),  Duration::from_millis(2000));  // passthrough
    }

    #[test]
    fn tasks_cancel_request_shape() {
        let msg = tasks_cancel_request(42, "job-1");
        assert_eq!(msg["jsonrpc"], "2.0");
        assert_eq!(msg["id"], 42);
        assert_eq!(msg["method"], "tasks/cancel");
        assert_eq!(msg["params"]["taskId"], "job-1");
    }

    #[test]
    fn extract_call_result_recognises_task_handle() {
        let result = json!({ "taskId": "job-9", "status": "working", "ttl": 120000 });
        match extract_call_result(&result) {
            McpCallResult::Task(t) => {
                assert_eq!(t.task_id, "job-9");
                assert_eq!(t.status, TaskStatus::Working);
                assert_eq!(t.ttl_ms, Some(120000));
            }
            other => panic!("expected Task, got {other:?}"),
        }
    }

    #[test]
    fn resource_link_passes_through_without_fetch() {
        let result = json!({
            "content": [ { "type": "resource_link", "uri": "https://x/y.mp4", "mimeType": "video/mp4" } ]
        });
        match extract_call_result(&result) {
            McpCallResult::Media { items, .. } => match &items[0].data {
                McpMediaData::Link { uri, mime } => {
                    assert_eq!(uri, "https://x/y.mp4");
                    assert_eq!(mime.as_deref(), Some("video/mp4"));
                }
                _ => panic!("expected link"),
            },
            other => panic!("expected Media, got {other:?}"),
        }
    }
}
