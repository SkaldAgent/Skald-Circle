//! The `Tool` trait, the type-erased [`ToolCtx`] (blueprint D3 — a type-map,
//! axum/tower style, not generics), and the cancellable execution machinery
//! (ported verbatim from Skald's core-api: it was already pure).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::ids::{ConversationId, FrameId, ToolCallId};

// ── Extensions ───────────────────────────────────────────────────────────────

/// A type-map of host values threaded into every tool call (axum/tower
/// style). Hosts insert in ONE place (turn construction) and read with typed
/// helpers — never scattered string keys.
#[derive(Clone, Default)]
pub struct Extensions {
    map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Extensions {
    pub fn new() -> Self { Self::default() }

    pub fn insert<T: Send + Sync + 'static>(&mut self, value: Arc<T>) -> &mut Self {
        self.map.insert(TypeId::of::<T>(), value);
        self
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.map.get(&TypeId::of::<T>())?.clone().downcast::<T>().ok()
    }

    pub fn contains<T: Send + Sync + 'static>(&self) -> bool {
        self.map.contains_key(&TypeId::of::<T>())
    }
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Extensions({} entries)", self.map.len())
    }
}

// ── ToolCtx ──────────────────────────────────────────────────────────────────

/// Per-invocation execution context threaded into a tool call.
#[derive(Clone)]
pub struct ToolCtx {
    pub conversation: ConversationId,
    pub frame:        FrameId,
    /// Agent of the current frame (self-call check for delegation).
    pub agent:        String,
    /// The call being executed (parent_call of any child frame).
    pub call_id:      ToolCallId,
    pub cancel:       CancellationToken,
    pub extensions:   Extensions,
}

// ── ToolOutput / ToolFailure ─────────────────────────────────────────────────

/// A reference to one media file a tool produced. The assembler decides
/// whether to inline it — the kernel only transports it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MediaRef {
    /// Absolute host path, already containment-checked by the producing tool.
    pub host_path: String,
    /// Sniffed MIME (informational — pipelines re-sniff from bytes).
    pub mime:      String,
}

/// The successful output of a tool.
#[derive(Debug, Clone)]
pub enum ToolOutput {
    Text(String),
    Json(Value),
    /// A text note plus media refs; the wire message carries only `text`.
    Media { text: String, refs: Vec<MediaRef> },
}

impl ToolOutput {
    /// Canonical string form persisted as the call result and replayed to the
    /// model (both OpenAI and Anthropic encode tool results as text/JSON).
    pub fn to_wire(&self) -> String {
        match self {
            Self::Text(s)          => s.clone(),
            Self::Json(v)          => serde_json::to_string(v).unwrap_or_else(|_| "null".into()),
            Self::Media { text, .. } => text.clone(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) | Self::Media { .. } => "string",
            Self::Json(_)                      => "json",
        }
    }

    pub fn media(&self) -> &[MediaRef] {
        match self {
            Self::Media { refs, .. } => refs,
            _                        => &[],
        }
    }
}

impl From<String> for ToolOutput {
    fn from(s: String) -> Self { Self::Text(s) }
}
impl From<&str> for ToolOutput {
    fn from(s: &str) -> Self { Self::Text(s.to_string()) }
}

/// How a tool call can fail.
#[derive(Debug, Clone)]
pub enum ToolFailure {
    Failed(String),
    /// The tool suspended waiting for a human and the channel closed: the turn
    /// ends, the call STAYS `AwaitingHuman` for the resume. (The tool marks
    /// the call `AwaitingHuman` via the store BEFORE returning this.)
    Suspend,
}

impl std::fmt::Display for ToolFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(e) => write!(f, "{e}"),
            Self::Suspend   => write!(f, "tool suspended awaiting human input"),
        }
    }
}

impl std::error::Error for ToolFailure {}

// ── RestartHint / Visibility ─────────────────────────────────────────────────

/// What recovery does with a call that was `Running` at crash (blueprint D7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RestartHint {
    /// Re-gate and re-execute (default — today's behavior; idempotent tools).
    #[default]
    ReExecute,
    /// Resolve as Failed "interrupted" (tools with non-idempotent external
    /// side effects, e.g. shell commands).
    MarkInterrupted,
}

/// Declared visibility — the HOST filters at `ToolSet` construction, the
/// kernel never filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    #[default]
    Always,
    InteractiveOnly,
    RootOnly,
    SubAgentsOnly,
}

// ── Tool ─────────────────────────────────────────────────────────────────────

/// A single LLM-callable tool.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    /// OpenAI-shaped tool definition (`{"type":"function","function":{…}}`).
    fn definition(&self) -> Value;

    /// The simple execution path. The kernel wraps it in a [`SimpleExecution`]
    /// by default (drop of the future = stop) — override [`start`](Self::start)
    /// for remote/child teardown instead.
    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<crate::tool::ToolOutput, ToolFailure>;

    /// May this call run in parallel with other concurrency-safe calls of the
    /// same round? (Generalized sub-agent batch, blueprint §7.) Default false
    /// → the sequential path.
    fn concurrency_safe(&self, _args: &Value) -> bool { false }

    /// Recovery behavior when the call was `Running` at crash (D7).
    fn restart_hint(&self) -> RestartHint { RestartHint::ReExecute }

    /// Declared visibility (host-side filtering only).
    fn visibility(&self) -> Visibility { Visibility::Always }

    /// Start one execution, returning a live handle. The default wraps
    /// [`call`](Self::call) in a [`SimpleExecution`]. Tools needing
    /// remote/child teardown (kill a process group, POST an /interrupt)
    /// override this with a bespoke [`ToolExecution::stop`].
    fn start<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> Box<dyn ToolExecution + 'a> {
        Box::new(SimpleExecution::new(Box::pin(self.call(args, ctx))))
    }
}

// ── ToolSet ──────────────────────────────────────────────────────────────────

/// The per-turn tool registry, ALREADY filtered by the host (visibility,
/// approval, interactive). `defs` is re-read at EVERY round and every
/// fallback attempt: grants activated at round N are visible at round N+1,
/// and a cross-mode DTL fallback re-shapes for free.
pub trait ToolSet: Send + Sync {
    fn defs(&self, model: &crate::model::ModelInfo) -> Vec<Value>;
    fn find(&self, name: &str) -> Option<Arc<dyn Tool>>;
}

/// Wrapper so `Arc<dyn ToolSet>` can ride in [`Extensions`] (type-map keys
/// must be `Sized`). The kernel inserts one into every `ToolCtx`; shipped
/// tools that spawn child loops (delegate) inherit from it.
#[derive(Clone)]
pub struct SharedToolSet(pub Arc<dyn ToolSet>);

/// A trivial `ToolSet` from a list of tools (testing, simple hosts).
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self { Self { tools: Vec::new() } }

    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    pub fn with_arc(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn into_toolset(self) -> Arc<dyn ToolSet> { Arc::new(self) }
}

impl Default for ToolRegistry {
    fn default() -> Self { Self::new() }
}

impl ToolSet for ToolRegistry {
    fn defs(&self, _model: &crate::model::ModelInfo) -> Vec<Value> {
        self.tools.iter().map(|t| t.definition()).collect()
    }

    fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }
}

// ── ToolExecution ────────────────────────────────────────────────────────────

/// Lifecycle state of a single tool execution (in-memory, richer than the
/// persisted `CallState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolExecutionState {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Terminal outcome of [`ToolExecution::wait`].
#[derive(Debug, Clone)]
pub enum ExecutionOutcome {
    Completed(ToolOutput),
    Failed(String),
    Cancelled,
    /// The tool suspended awaiting a human (`ToolFailure::Suspend`): the turn
    /// ends and the call STAYS `AwaitingHuman` — never resolve it here.
    Suspended,
}

impl ExecutionOutcome {
    pub fn into_call_outcome(self) -> crate::store::CallOutcome {
        match self {
            Self::Completed(out) => crate::store::CallOutcome::Completed(out),
            Self::Failed(e)      => crate::store::CallOutcome::Failed(e),
            Self::Cancelled      => crate::store::CallOutcome::Cancelled,
            // Handled by the kernel before this conversion is reached.
            Self::Suspended      => crate::store::CallOutcome::Cancelled,
        }
    }
}

/// A single live execution of a [`Tool`]. Pure: it never touches a store or a
/// transport — the kernel mirrors transitions to persistence and events.
pub trait ToolExecution: Send + Sync {
    fn state(&self) -> ToolExecutionState;
    /// Drive the work to its terminal outcome. Called exactly once.
    fn wait<'a>(&'a self) -> Pin<Box<dyn Future<Output = ExecutionOutcome> + Send + 'a>>;
    /// Tool-specific cancellation. The default relies on the driver dropping
    /// the `wait` future.
    fn stop<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
}

/// The boxed work unit inside a [`SimpleExecution`].
pub type ToolWork<'a> =
    Pin<Box<dyn Future<Output = Result<ToolOutput, ToolFailure>> + Send + 'a>>;

/// Default [`ToolExecution`] for any tool that is a single async unit of work:
/// `wait` races the work against a stop-token, so `stop()` (or dropping
/// `wait`) aborts the in-flight I/O.
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

impl ToolExecution for SimpleExecution<'_> {
    fn state(&self) -> ToolExecutionState { *self.state.lock().unwrap() }

    fn wait<'b>(&'b self) -> Pin<Box<dyn Future<Output = ExecutionOutcome> + Send + 'b>> {
        Box::pin(async move {
            let work = self.work.lock().await.take();
            let Some(work) = work else { return ExecutionOutcome::Cancelled };
            let outcome = tokio::select! {
                biased;
                _ = self.stop.cancelled() => ExecutionOutcome::Cancelled,
                r = work => match r {
                    Ok(out)          => ExecutionOutcome::Completed(out),
                    Err(ToolFailure::Failed(e)) => ExecutionOutcome::Failed(e),
                    Err(ToolFailure::Suspend)   => ExecutionOutcome::Suspended,
                },
            };
            *self.state.lock().unwrap() = match outcome {
                ExecutionOutcome::Completed(_) => ToolExecutionState::Completed,
                ExecutionOutcome::Failed(_)    => ToolExecutionState::Failed,
                ExecutionOutcome::Cancelled | ExecutionOutcome::Suspended => ToolExecutionState::Cancelled,
            };
            outcome
        })
    }

    fn stop<'b>(&'b self) -> Pin<Box<dyn Future<Output = ()> + Send + 'b>> {
        Box::pin(async move { self.stop.cancel() })
    }
}

/// Run a [`ToolExecution`] to completion honouring a cancellation token: on
/// cancel, `exec.stop()` is called once (tool-specific teardown), then `wait`
/// resolves.
pub async fn drive_execution(exec: &dyn ToolExecution, cancel: &CancellationToken) -> ExecutionOutcome {
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
