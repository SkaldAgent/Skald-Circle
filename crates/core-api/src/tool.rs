use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

// ── ToolDescriptionLength ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolDescriptionLength {
    Short,
    Full,
}

// ── ToolCategory ──────────────────────────────────────────────────────────────

/// Logical grouping for a tool.
///
/// Used for access-control filtering and for display/audit purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    /// Read or write files on disk.
    Filesystem,
    /// Run shell commands or restart the process.
    Shell,
    /// Invoke sub-agents via call_agent.
    Subagent,
    /// Read-only discovery of system state.
    Introspection,
    /// Mutate system configuration.
    Config,
}

// ── ToolContext ───────────────────────────────────────────────────────────────

/// Per-invocation execution context threaded into a tool's [`Tool::run_with`].
///
/// Carries the identity of the session driving the call and the owner's database
/// pool. Owner-bound tools (e.g. cron management) read `pool` to act on the
/// caller's own `{userid}.db` rather than a manager captured globally at
/// registration time. Context-free tools ignore it — the default `run_with`
/// delegates to `run`, so most tools need no change.
#[derive(Clone)]
pub struct ToolContext {
    /// The session that issued this tool call. Ids are local to `pool`.
    pub session_id: i64,
    /// The owner (caller) user id. Tools that address a per-user external store
    /// (e.g. the Honcho memory peer) key on this so they act on the caller's own
    /// data, never a shared/global peer.
    pub user_id: String,
    /// The owner's unlocked database pool (per-user in multi-user mode; the shared
    /// `system.db` in the transitional single-pool state).
    pub pool: Arc<sqlx::SqlitePool>,
    /// The caller's filesystem view (blueprint §6): private home + shared folders +
    /// the container they resolve into. `execute_cmd` execs into `fs.container_name`
    /// and the disk fs-tools resolve physical paths against `fs`'s host bases.
    pub fs: Arc<crate::user_fs::UserFs>,
}

// ── Tool trait ────────────────────────────────────────────────────────────────

/// A single LLM-callable tool.
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;

    /// Human-readable label for this tool invocation shown in UI / notifications.
    fn describe(&self, _args: &Value, _length: ToolDescriptionLength) -> String {
        self.name().to_string()
    }

    /// Friendly, **static** display name for this tool ("Edit File", "Read File"),
    /// shown as the card title in the chat UI — separate from [`name`](Self::name),
    /// which stays the raw LLM function id. Several tools map to the same friendly
    /// verb (e.g. `write_file`/`edit_file`/`insert_at_line` → "Edit File"). The
    /// default returns the raw name so unmapped tools still render something.
    fn display_name(&self) -> &str {
        self.name()
    }

    /// Semantic icon key for the chat card — **not** a glyph. The frontend maps the
    /// key to a concrete icon + accent color (themeable), so the core commits to a
    /// meaning, never a look. Known keys: `edit`, `read`, `list`, `search`, `shell`,
    /// `subagent`, `image`, `config`, `introspection`. The default derives from
    /// [`category`](Self::category).
    fn icon(&self) -> &str {
        match self.category() {
            ToolCategory::Filesystem    => "file",
            ToolCategory::Shell         => "shell",
            ToolCategory::Subagent      => "subagent",
            ToolCategory::Introspection => "introspection",
            ToolCategory::Config        => "config",
        }
    }

    /// If this invocation targets a single file the user can open in the file
    /// viewer, return its path (relative to the project root, or absolute).
    /// Tools that target a directory (list/grep) or no file at all return
    /// `None`. The frontend renders the returned path as a clickable link.
    fn target_path(&self, _args: &Value) -> Option<String> {
        None
    }

    /// JSON Schema for the `parameters` field in the OpenAI function definition.
    fn parameters_schema(&self) -> Value;

    /// Execute the tool synchronously and return a plain-text result (or error string).
    /// Tools that require async I/O should override `execute_async` instead.
    fn execute(&self, _args: Value) -> Result<String> {
        Err(anyhow::anyhow!("tool '{}': sync execute not implemented — use execute_async", self.name()))
    }

    /// Execute the tool asynchronously. The default wraps `execute`; async tools
    /// (e.g. image generation) override this directly to avoid `block_in_place`.
    fn execute_async<'a>(&'a self, args: Value) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move { self.execute(args) })
    }

    /// Execute and produce a typed [`ToolResult`]. The default bridges
    /// [`execute_async`](Self::execute_async) (plain string) into [`ToolResult::Text`],
    /// so existing tools need no changes. Override to return [`ToolResult::Json`]
    /// for tools with structured output (e.g. MCP tools exposing `structuredContent`).
    fn execute_typed<'a>(&'a self, args: Value)
        -> Pin<Box<dyn Future<Output = Result<ToolResult>> + Send + 'a>> {
        Box::pin(async move { Ok(ToolResult::Text(self.execute_async(args).await?)) })
    }

    /// Start a single execution of this tool and return a live [`ToolExecution`]
    /// handle. This is the entry point the session driver uses: it lets the tool
    /// own its in-flight state and implement its own `stop()` (e.g. ComfyUI sends
    /// an `/interrupt`; `execute_cmd` relies on `kill_on_drop`).
    ///
    /// The default wraps `execute_typed` in a [`SimpleExecution`], whose `stop()`
    /// drops the work future — already enough to make `/stop` responsive for any
    /// I/O-bound tool. Tools needing remote/child teardown override this and
    /// return their own `ToolExecution` with a bespoke `stop()`.
    fn run<'a>(&'a self, args: Value) -> Box<dyn ToolExecution + 'a> {
        Box::new(SimpleExecution::new(self.execute_typed(args)))
    }

    /// Context-aware variant of [`run`](Self::run): the session driver threads a
    /// [`ToolContext`] (session id + owner pool) so owner-bound tools can act on
    /// the caller's own database instead of a globally-captured manager. The
    /// default ignores the context and delegates to `run`, so context-free tools
    /// need no change.
    fn run_with<'a>(&'a self, _ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        self.run(args)
    }

    /// Logical category of this tool.
    fn category(&self) -> ToolCategory;

    /// If true, this tool is only included in the tool list for sub-agents (depth > 0).
    fn sub_agents_only(&self) -> bool { false }

    /// If true, this tool is only included in the tool list for the root agent (depth == 0).
    fn root_agent_only(&self) -> bool { false }

    /// If true, this tool is only available to interactive sessions (web, telegram, mobile, voice).
    /// Non-interactive background sessions (cron, event-triage) will not receive
    /// this tool definition.
    fn interactive_only(&self) -> bool { false }

    /// Full OpenAI-format tool definition ready to be sent to the LLM.
    fn openai_definition(&self) -> Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name":        self.name(),
                "description": self.description(),
                "parameters":  self.parameters_schema(),
            }
        })
    }
}

// ── ToolExecutionState ────────────────────────────────────────────────────────

/// Lifecycle state of a single tool execution.
///
/// Richer than the persisted `chat_llm_tools.status` string: it distinguishes a
/// user `/stop` (`Cancelled`) and a policy/human denial (`Rejected`) from a real
/// tool error (`Failed`). The session driver owns the approval-phase states
/// (`Pending`, `AwaitingApproval`, `Rejected`); a [`ToolExecution`] itself only
/// ever reports `Running → Completed | Failed | Cancelled`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionState {
    /// Intent recorded, not yet started (transient; not persisted on its own).
    Pending,
    /// Blocked waiting for a human approval / clarification answer.
    AwaitingApproval,
    /// Actively executing.
    Running,
    /// Finished successfully.
    Completed,
    /// Finished with a tool/runtime error.
    Failed,
    /// Stopped by the user via `/stop` — not an error.
    Cancelled,
    /// Denied by an approval policy or a human — not an error.
    Rejected,
}

impl ToolExecutionState {
    /// String persisted in `chat_llm_tools.status`. `AwaitingApproval` maps to the
    /// legacy `pending` value so existing resume logic keeps working; the brand-new
    /// `Pending` state is never persisted (the row is created on the first real
    /// transition) and defaults to `running` defensively.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Pending          => "running",
            Self::AwaitingApproval => "pending",
            Self::Running          => "running",
            Self::Completed        => "done",
            Self::Failed           => "failed",
            Self::Cancelled        => "cancelled",
            Self::Rejected         => "rejected",
        }
    }
}

// ── ToolResult ────────────────────────────────────────────────────────────────

/// The typed result of a successful tool execution.
///
/// Most tools produce plain text ([`ToolResult::Text`], persisted as
/// `result_type = "string"`). Tools with structured output — notably MCP servers
/// returning `structuredContent` — produce [`ToolResult::Json`] (persisted as
/// `result_type = "json"`), so the host/frontend can render the typed payload
/// instead of a raw text blob. At the LLM wire both variants become the same
/// `{"role":"tool","content": <string>}` bytes (see [`ToolResult::to_wire`]); the
/// type tag only matters to the host.
#[derive(Debug, Clone)]
pub enum ToolResult {
    /// Plain-text result. The default for every built-in tool.
    Text(String),
    /// Structured JSON result (e.g. MCP `structuredContent`).
    Json(serde_json::Value),
    /// A text note plus one or more media files the model may view natively
    /// (image / video / PDF). At the LLM wire the `tool` message carries only
    /// `text` (see [`to_wire`](Self::to_wire)); the media travels **out of band**
    /// (persisted in `chat_llm_tools.media`) and is inlined by the message
    /// builder as a following synthetic `user` message — but only for the
    /// current turn and only when the resolved model declares the modality;
    /// otherwise it is silently dropped and the note stands alone.
    Media { text: String, media: Vec<MediaRef> },
}

impl ToolResult {
    /// Tag persisted in `chat_llm_tools.result_type` and sent over the WS as
    /// `ServerEvent::ToolDone.result_type`. `Media` reports `"string"`: its wire
    /// form *is* a plain text note, and the media is signalled out of band by the
    /// `chat_llm_tools.media` column — so the frontend needs no new result type.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text(_)      => "string",
            Self::Json(_)      => "json",
            Self::Media { .. } => "string",
        }
    }

    /// Wire content for the LLM tool message: text as-is, Json serialized to a
    /// compact JSON string, `Media` its text note. Both OpenAI and Anthropic
    /// encode tool results as text/JSON, so this is the canonical string form
    /// persisted in `chat_llm_tools.result` and replayed by the message builder.
    pub fn to_wire(&self) -> String {
        match self {
            Self::Text(s)          => s.clone(),
            Self::Json(v)          => serde_json::to_string(v).unwrap_or_else(|_| "null".to_string()),
            Self::Media { text, .. } => text.clone(),
        }
    }

    /// The media files this result carries (empty for `Text`/`Json`). The message
    /// builder reads these to inline the files as native model input.
    pub fn media(&self) -> &[MediaRef] {
        match self {
            Self::Media { media, .. } => media,
            _                         => &[],
        }
    }
}

/// A reference to one media file a tool produced (e.g. `read_file` on an image).
/// Carries the **already-containment-checked** absolute host path so the message
/// builder can re-read + inline it, plus the sniffed MIME for display. Serialized
/// as JSON into the `chat_llm_tools.media` column.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaRef {
    /// Absolute host path, resolved and containment-checked by the producing tool.
    pub host_path: String,
    /// Sniffed MIME type (`image/png`, `application/pdf`, …). Informational — the
    /// media pipeline re-sniffs from the bytes before inlining, never trusting this.
    pub mime: String,
}

impl From<String> for ToolResult {
    fn from(s: String) -> Self { Self::Text(s) }
}

impl From<&str> for ToolResult {
    fn from(s: &str) -> Self { Self::Text(s.to_string()) }
}

// ── ExecutionOutcome ──────────────────────────────────────────────────────────

/// Terminal outcome produced by [`ToolExecution::wait`]. A running execution can
/// only end in one of these three ways; `Rejected`/`AwaitingApproval` are decided
/// by the approval gate *before* the work runs and never appear here.
#[derive(Debug, Clone)]
pub enum ExecutionOutcome {
    Completed(ToolResult),
    Failed(String),
    Cancelled,
}

impl ExecutionOutcome {
    pub fn state(&self) -> ToolExecutionState {
        match self {
            Self::Completed(_) => ToolExecutionState::Completed,
            Self::Failed(_)    => ToolExecutionState::Failed,
            Self::Cancelled    => ToolExecutionState::Cancelled,
        }
    }
}

/// A boxed, owned unit of asynchronous tool work producing a typed [`ToolResult`].
/// This is what [`Tool::execute_typed`] returns; [`SimpleExecution`] wraps one.
pub type ToolWork<'a> = Pin<Box<dyn Future<Output = Result<ToolResult>> + Send + 'a>>;

// ── ToolExecution ─────────────────────────────────────────────────────────────

/// A single, live execution of a [`Tool`]. Owns its in-memory state and decides
/// how it stops. Pure: it never touches the DB or the WebSocket — the session
/// driver mirrors its state transitions to persistence and transport.
///
/// `Send + Sync` is required because the driver shares `&self` across the two
/// concurrent branches of the cancellation race (`wait` vs `stop`).
pub trait ToolExecution: Send + Sync {
    /// Current in-memory lifecycle state.
    fn state(&self) -> ToolExecutionState;

    /// Drive the work to its terminal outcome. Called exactly once by the driver.
    fn wait<'a>(&'a self) -> Pin<Box<dyn Future<Output = ExecutionOutcome> + Send + 'a>>;

    /// Tool-specific cancellation: signal the work to stop and tear down any
    /// remote/child resources. The default relies on the driver dropping the
    /// `wait` future; [`SimpleExecution`] overrides it to cancel its stop-token.
    fn stop<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

// ── SimpleExecution ───────────────────────────────────────────────────────────

/// Default [`ToolExecution`] for any tool that is a single async unit of work.
///
/// Holds the work future plus a stop-token; `wait` races the two, so a `stop()`
/// (or the driver dropping `wait`) drops the work future — aborting the in-flight
/// I/O (a `reqwest` connection, a `kill_on_drop` child, …). Enough to make
/// `/stop` responsive for every I/O-bound tool with zero per-tool code.
pub struct SimpleExecution<'a> {
    state: Mutex<ToolExecutionState>,
    stop:  CancellationToken,
    work:  tokio::sync::Mutex<Option<ToolWork<'a>>>,
}

impl<'a> SimpleExecution<'a> {
    pub fn new(work: ToolWork<'a>) -> Self {
        Self {
            state: Mutex::new(ToolExecutionState::Running),
            stop:  CancellationToken::new(),
            work:  tokio::sync::Mutex::new(Some(work)),
        }
    }
}

impl<'a> ToolExecution for SimpleExecution<'a> {
    fn state(&self) -> ToolExecutionState {
        *self.state.lock().unwrap()
    }

    fn wait<'b>(&'b self) -> Pin<Box<dyn Future<Output = ExecutionOutcome> + Send + 'b>> {
        Box::pin(async move {
            let work = self.work.lock().await.take();
            let Some(work) = work else {
                // Already consumed (e.g. a second wait after cancellation).
                return ExecutionOutcome::Cancelled;
            };
            let outcome = tokio::select! {
                biased;
                _ = self.stop.cancelled() => ExecutionOutcome::Cancelled,
                r = work => match r {
                    Ok(s)  => ExecutionOutcome::Completed(s),
                    Err(e) => ExecutionOutcome::Failed(e.to_string()),
                },
            };
            *self.state.lock().unwrap() = outcome.state();
            outcome
        })
    }

    fn stop<'b>(&'b self) -> Pin<Box<dyn Future<Output = ()> + Send + 'b>> {
        Box::pin(async move { self.stop.cancel(); })
    }
}

// ── drive_execution ───────────────────────────────────────────────────────────

/// Run a [`ToolExecution`] to completion while honouring a cancellation token.
///
/// When `cancel` fires we call `exec.stop()` once (tool-specific teardown); the
/// execution then resolves `wait` to `Cancelled`. Both methods take `&self`, so
/// the two concurrent borrows are shared and the borrow checker is happy.
pub async fn drive_execution(
    exec:   &dyn ToolExecution,
    cancel: &CancellationToken,
) -> ExecutionOutcome {
    let work = exec.wait();
    tokio::pin!(work);

    let mut stopped = false;
    loop {
        tokio::select! {
            biased;
            outcome = &mut work => return outcome,
            _ = cancel.cancelled(), if !stopped => {
                exec.stop().await;
                stopped = true;
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Truncates a label to `max` chars, appending `…` if cut.
pub fn truncate_label(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut = max.saturating_sub(1);
    let mut end = cut;
    while !s.is_char_boundary(end) { end -= 1; }
    format!("{}…", &s[..end])
}
