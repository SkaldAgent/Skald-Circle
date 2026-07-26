//! Sub-agents as a tool (blueprint §7, D2): the kernel never intercepts
//! anything — `delegate` is a tool like any other, dispatched through the
//! normal gate/hooks/execution path. A sync child is just a slow tool call the
//! parent awaits; a homogeneous batch of sync delegates fans out through the
//! kernel's generic concurrency (`concurrency_safe`).
//!
//! Both flows ship. A SYNC child is awaited in place; an ASYNC one is handed to
//! the host's [`AsyncExecutor`] and its result comes back later through an
//! [`AsyncResultSink`] — a tool call the model already has an id for, resolved
//! whenever the work finishes.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::async_trait;
use crate::context::SystemContextSource;
use crate::events::{EventSink, LoopEvent};
use crate::ids::{ConversationId, FrameId, TaskId, ToolCallId};
use crate::manager::{LoopManager, LoopParams, TurnMeta};
use crate::model::{ModelHint, ModelSelector};
use crate::store::{CallOutcome, FrameSpec, HistoryStore, NewCall, NewMessage};
use crate::tool::{Extensions, SharedToolSet, Tool, ToolCtx, ToolFailure, ToolOutput, ToolSet};

// ── AgentCatalog ─────────────────────────────────────────────────────────────

/// The agent's kind (from the host's meta). Only `Task` agents are
/// dispatchable via `delegate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    Chat,
    Task,
    System,
}

/// A dispatchable agent.
#[derive(Clone)]
pub struct AgentProfile {
    pub id:       String,
    pub kind:     AgentKind,
    /// The child's system context (its own prompt — never the parent's, B3).
    pub context:  Arc<dyn SystemContextSource>,
    /// How the child's tool set derives from the parent's (ignored when
    /// `toolset` is set).
    pub tools:    ToolSelection,
    /// Full tool-set override (hosts whose children need a fresh registry
    /// rather than a filtered view of the parent's — e.g. fresh grant sets).
    pub toolset:  Option<Arc<dyn ToolSet>>,
    /// Model pin (bypasses AUTO). Strength is resolved by the host's selector.
    pub model:    Option<ModelHint>,
    /// Per-child selector override (e.g. a different required strength, D14).
    pub selector: Option<Arc<dyn ModelSelector>>,
    /// Per-child assembler override (e.g. scoped DTL activation).
    pub assembler: Option<Arc<dyn crate::context::ContextAssembler>>,
}

/// How a child's tool set derives from the parent's: strip `remove` by name,
/// then append `add`.
#[derive(Clone, Default)]
pub struct ToolSelection {
    pub remove: Vec<String>,
    pub add:    Vec<Arc<dyn Tool>>,
}

impl ToolSelection {
    pub fn inherit() -> Self { Self::default() }
    pub fn minus(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self { remove: names.into_iter().map(Into::into).collect(), add: Vec::new() }
    }
    pub fn plus(tools: Vec<Arc<dyn Tool>>) -> Self {
        Self { remove: Vec::new(), add: tools }
    }
}

/// Summary for catalog listings (a future `list_agents` tool).
#[derive(Debug, Clone)]
pub struct AgentSummary {
    pub id:          String,
    pub kind:        AgentKind,
    pub description: String,
}

#[async_trait]
pub trait AgentCatalog: Send + Sync {
    /// Load a dispatchable profile, built for `child_frame` (already opened by
    /// the DelegateTool — frame-scoped pieces like grants/activation anchor to
    /// it). MUST reject non-`Task` kinds and unknown ids.
    ///
    /// `ctx` is the delegating call's context: a catalog that lives as long as
    /// the tenant reads the turn's own state (session, source, permissions)
    /// from `ctx.extensions` instead of having captured it at construction.
    async fn get(
        &self,
        id:          &str,
        child_frame: FrameId,
        ctx:         &ToolCtx,
    ) -> crate::Result<AgentProfile>;
    async fn list(&self, kind: AgentKind) -> Vec<AgentSummary>;
    /// Frame-exit hook (host cleanup, e.g. deleting stack-scoped activations).
    async fn on_child_closed(&self, _frame: crate::ids::FrameId) {}
}

// ── FilteredToolSet ──────────────────────────────────────────────────────────

/// The child's tool set: parent's minus `remove`, plus `add`.
pub struct FilteredToolSet {
    inner:  Arc<dyn ToolSet>,
    remove: Vec<String>,
    add:    Vec<Arc<dyn Tool>>,
}

impl FilteredToolSet {
    /// A child's set derived from the parent's. Used by the delegate at
    /// dispatch and by [`crate::recovery`] when it rebuilds a resumed frame.
    pub fn derive(inner: Arc<dyn ToolSet>, selection: &ToolSelection) -> Self {
        Self {
            inner,
            remove: selection.remove.clone(),
            add:    selection.add.clone(),
        }
    }
}

impl ToolSet for FilteredToolSet {
    fn defs(&self, model: &crate::model::ModelInfo) -> Vec<Value> {
        let mut defs: Vec<Value> = self
            .inner
            .defs(model)
            .into_iter()
            .filter(|d| {
                let name = d["function"]["name"].as_str().unwrap_or("");
                !self.remove.iter().any(|r| r == name)
            })
            .collect();
        defs.extend(self.add.iter().map(|t| t.definition()));
        defs
    }

    fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if let Some(t) = self.add.iter().find(|t| t.name() == name) {
            return Some(t.clone());
        }
        if self.remove.iter().any(|r| r == name) {
            return None;
        }
        self.inner.find(name)
    }
}

// ── Async delegation ─────────────────────────────────────────────────────────

/// What the host is asked to run out of band (blueprint §7.2).
///
/// The parent's turn does **not** wait for it: `delegate` returns a receipt and
/// the loop moves on. Everything needed to run the work later is in here, so an
/// executor backed by a durable queue can pick it up after a restart.
#[derive(Clone)]
pub struct AsyncSpec {
    pub conversation: ConversationId,
    /// The delegating frame — where the result is delivered.
    pub parent_frame: FrameId,
    /// The delegating call, so a host can correlate its own record with ours.
    pub parent_call:  ToolCallId,
    /// The agent that delegated (the child's is `agent`).
    pub parent_agent: String,
    pub agent:        String,
    pub prompt:       String,
    pub title:        Option<String>,
    pub description:  Option<String>,
    /// The delegating turn's extensions (the host's own context).
    pub extensions:   Extensions,
}

/// The host's receipt for a submitted task.
#[derive(Debug, Clone)]
pub struct TaskHandle {
    pub id:    TaskId,
    pub title: String,
}

/// Runs a delegated task out of band. **Durability is the host's**: the crate's
/// [`InProcessExecutor`] is lossy across restarts, a queue-backed one is not.
#[async_trait]
pub trait AsyncExecutor: Send + Sync {
    async fn submit(&self, spec: AsyncSpec) -> crate::Result<TaskHandle>;
}

/// A task that finished, whatever ran it.
#[derive(Debug, Clone)]
pub struct CompletedTask {
    pub id:     TaskId,
    pub title:  String,
    pub result: String,
}

/// Where a finished task's result goes.
#[async_trait]
pub trait AsyncResultSink: Send + Sync {
    async fn deliver(&self, parent: ConversationId, task: CompletedTask) -> crate::Result<()>;
}

/// The wire name of the synthetic call carrying a delivered result. The model
/// sees it as a tool call it never made — which is exactly what it is: the
/// system reporting back.
pub const DELIVERY_CALL: &str = "task_completed";

/// The shipped sink: writes the delivery into the store, as a synthetic
/// assistant message plus one completed call.
///
/// Durable by construction — it is a normal state transition, so the result is
/// in the history the instant it lands, whether or not anything is driving the
/// conversation. **Waking the parent is the host's job**: a live loop picks the
/// result up on its own (it reads the store each round), and an idle
/// conversation needs a resume, which only the host knows how to trigger for
/// its surfaces. Wrap this sink to add that.
pub struct StoreSink {
    store:     Arc<dyn HistoryStore>,
    call_name: String,
}

impl StoreSink {
    pub fn new(store: Arc<dyn HistoryStore>) -> Self {
        Self { store, call_name: DELIVERY_CALL.to_string() }
    }

    /// Rename the synthetic call (hosts with their own legacy name).
    pub fn with_call_name(mut self, name: impl Into<String>) -> Self {
        self.call_name = name.into();
        self
    }
}

#[async_trait]
impl AsyncResultSink for StoreSink {
    async fn deliver(&self, parent: ConversationId, task: CompletedTask) -> crate::Result<()> {
        // The deepest active frame is where the conversation currently is: a
        // result delivered to a closed frame would never be read.
        let frame = self
            .store
            .deepest_active(&parent)
            .await?
            .ok_or_else(|| anyhow::anyhow!("deliver: no active frame on conversation {parent}"))?;

        let reasoning = format!(
            "The system is notifying me that async task #{} ('{}') has completed. \
             Let me process the result via {}.",
            task.id, task.title, self.call_name,
        );
        let msg = self
            .store
            .append(
                frame.id,
                NewMessage {
                    role:      crate::store::Role::Assistant,
                    content:   String::new(),
                    synthetic: true,
                    reasoning: Some(reasoning),
                    metadata:  None,
                },
            )
            .await?;

        let call = self
            .store
            .append_call(msg, NewCall::new(&self.call_name, json!({ "task_id": task.id.get() })))
            .await?;
        let payload = json!({
            "task_id": task.id.get(),
            "title":   task.title,
            "result":  task.result,
        });
        self.store
            .resolve_call(call, &CallOutcome::Completed(ToolOutput::Text(payload.to_string())))
            .await?;
        Ok(())
    }
}

/// The lossy executor: runs the task on the current process, on the same
/// manager, and delivers through the given sink.
///
/// **A restart loses in-flight tasks** — nothing records that the work was
/// owed. Fine for a single-process host that treats async delegation as
/// best-effort; a host that must not lose one wires an executor over its own
/// durable queue (Skald: a `scheduled_jobs` row).
pub struct InProcessExecutor {
    manager:  Arc<LoopManager>,
    catalog:  Arc<dyn AgentCatalog>,
    store:    Arc<dyn HistoryStore>,
    sink:     Arc<dyn AsyncResultSink>,
    tools:    Arc<dyn ToolSet>,
    next_id:  std::sync::atomic::AtomicI64,
}

impl InProcessExecutor {
    pub fn new(
        manager: Arc<LoopManager>,
        catalog: Arc<dyn AgentCatalog>,
        store:   Arc<dyn HistoryStore>,
        sink:    Arc<dyn AsyncResultSink>,
        tools:   Arc<dyn ToolSet>,
    ) -> Self {
        Self { manager, catalog, store, sink, tools, next_id: std::sync::atomic::AtomicI64::new(1) }
    }
}

#[async_trait]
impl AsyncExecutor for InProcessExecutor {
    async fn submit(&self, spec: AsyncSpec) -> crate::Result<TaskHandle> {
        let id = TaskId(self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let title = spec.title.clone().unwrap_or_else(|| spec.agent.clone());

        // Its own frame, child of the delegating one: the task is a sub-agent
        // that nobody awaits.
        let parent = self
            .store
            .get_frame(spec.parent_frame)
            .await?
            .ok_or_else(|| anyhow::anyhow!("submit: parent frame not found"))?;
        let frame = self
            .store
            .open_frame(&spec.conversation, Some(spec.parent_frame), FrameSpec {
                agent:       spec.agent.clone(),
                prompt:      Some(spec.prompt.clone()),
                depth:       parent.spec.depth + 1,
                // NOT the delegating call: that one is already resolved with the
                // receipt, and recovery must not try to complete it twice.
                parent_call: None,
                meta:        Value::Null,
            })
            .await?;
        // The delegating call's context, minus its cancellation: the profile is
        // resolved against the turn that asked for the work.
        let ctx = ToolCtx {
            conversation: spec.conversation.clone(),
            frame:        spec.parent_frame,
            agent:        spec.parent_agent.clone(),
            call_id:      spec.parent_call,
            cancel:       tokio_util::sync::CancellationToken::new(),
            extensions:   spec.extensions.clone(),
        };
        let profile = self.catalog.get(&spec.agent, frame, &ctx).await?;
        self.store.append(frame, NewMessage::agent(&spec.prompt)).await?;

        let manager = self.manager.clone();
        let store   = self.store.clone();
        let catalog = self.catalog.clone();
        let sink    = self.sink.clone();
        let tools   = profile.toolset.clone().unwrap_or_else(|| self.tools.clone());
        let task_title = title.clone();
        tokio::spawn(async move {
            let outcome = match manager
                .start_loop(LoopParams {
                    conversation: spec.conversation.clone(),
                    frame,
                    parent_frame: Some(spec.parent_frame),
                    agent:        spec.agent.clone(),
                    system:       profile.context,
                    tools,
                    model_hint:   profile.model.unwrap_or_default(),
                    selector:     profile.selector,
                    // Detached from the parent turn: the point of async is that
                    // the parent's /stop does not kill the background work.
                    token:        None,
                    live_input:   None,
                    extensions:   spec.extensions.clone(),
                    meta:         TurnMeta::default(),
                    assembler:    profile.assembler,
                })
                .await
            {
                Ok(handle) => handle.join().await,
                Err(e)     => Err(anyhow::anyhow!("{e}")),
            };

            catalog.on_child_closed(frame).await;
            let _ = store.close_frame(frame).await;

            let result = match outcome {
                Ok(crate::kernel::TurnOutcome::Final { content, .. }) => content,
                Ok(crate::kernel::TurnOutcome::Cancelled) => "(cancelled)".to_string(),
                Ok(crate::kernel::TurnOutcome::Exhausted) => {
                    "(no output: tool-call round budget exhausted)".to_string()
                }
                Err(e) => format!("(failed: {e})"),
            };
            if let Err(e) = sink
                .deliver(spec.conversation.clone(), CompletedTask { id, title: task_title, result })
                .await
            {
                tracing::error!(task = %id, "async task delivery failed: {e}");
            }
        });

        Ok(TaskHandle { id, title })
    }
}

// ── DelegateTool ─────────────────────────────────────────────────────────────

/// The shipped `delegate` tool. The parent loop simply awaits a slow tool —
/// nesting is reconstructed by subscribers from the `parent_frame` event tags.
#[derive(Clone)]
pub struct DelegateTool {
    manager:   Arc<LoopManager>,
    catalog:   Arc<dyn AgentCatalog>,
    store:     Arc<dyn HistoryStore>,
    max_depth: u32,
    name:      String,
    definition_override: Option<Value>,
    /// `None` → `mode: "async"` is refused instead of silently running sync.
    async_exec: Option<Arc<dyn AsyncExecutor>>,
}

impl DelegateTool {
    pub fn new(
        manager:   Arc<LoopManager>,
        catalog:   Arc<dyn AgentCatalog>,
        store:     Arc<dyn HistoryStore>,
        max_depth: u32,
    ) -> Self {
        Self {
            manager,
            catalog,
            store,
            max_depth,
            name: "delegate".to_string(),
            definition_override: None,
            async_exec: None,
        }
    }

    /// Wire `mode: "async"` to an executor. Without one the mode is refused —
    /// running it synchronously instead would block a turn that asked not to
    /// wait.
    pub fn with_async(mut self, exec: Arc<dyn AsyncExecutor>) -> Self {
        self.async_exec = Some(exec);
        self
    }

    /// Register under a different wire name (Skald's legacy aliases
    /// `execute_task` / `execute_subtask`, blueprint D11).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Override the advertised definition (legacy aliases keep their exact
    /// legacy schema byte-for-byte).
    pub fn with_definition(mut self, def: Value) -> Self {
        self.definition_override = Some(def);
        self
    }

    /// The schema: `agent_id` + `prompt` required; `title`, `description`,
    /// `mode` ("sync" — async rides the host executor), `client` accepted for
    /// legacy compatibility.
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id":    { "type": "string", "description": "Id of the task agent to delegate to" },
                "prompt":      { "type": "string", "description": "The full brief for the sub-agent" },
                "title":       { "type": "string", "description": "Optional short title for the task" },
                "description": { "type": "string", "description": "Optional longer description" },
                "mode":        { "type": "string", "enum": ["sync", "async"],
                                 "description": "sync: wait for the result. async: host-scheduled (if wired)" },
                "client":      { "type": "string", "description": "Optional model override" }
            },
            "required": ["agent_id", "prompt"]
        })
    }

    /// Hands the work to the host and returns the receipt immediately. The
    /// result arrives later as its own call (see [`AsyncResultSink`]), so the
    /// model is told plainly not to poll for it.
    async fn run_async(
        &self,
        agent_id: &str,
        prompt:   &str,
        args:     &Value,
        ctx:      &ToolCtx,
    ) -> Result<ToolOutput, ToolFailure> {
        let Some(exec) = &self.async_exec else {
            return Err(ToolFailure::Failed(
                "delegate: async mode is not available in this session".to_string(),
            ));
        };
        if agent_id == ctx.agent {
            return Err(ToolFailure::Failed(format!(
                "delegate: an agent cannot call itself (`{agent_id}`)"
            )));
        }

        let handle = exec
            .submit(AsyncSpec {
                conversation: ctx.conversation.clone(),
                parent_frame: ctx.frame,
                parent_call:  ctx.call_id,
                parent_agent: ctx.agent.clone(),
                agent:        agent_id.to_string(),
                prompt:       prompt.to_string(),
                title:        args["title"].as_str().map(str::to_string),
                description:  args["description"].as_str().map(str::to_string),
                extensions:   ctx.extensions.clone(),
            })
            .await
            .map_err(|e| ToolFailure::Failed(format!("delegate: async submit failed: {e}")))?;

        Ok(ToolOutput::Text(
            json!({
                "task_id": handle.id.get(),
                "status":  "started",
                "message": format!(
                    "Task {} ('{}') is running in the background. \
                     The system will automatically deliver the result to this conversation when complete. \
                     Do NOT poll for it. Continue the conversation normally.",
                    handle.id, handle.title,
                ),
            })
            .to_string(),
        ))
    }

    async fn run_sync(&self, agent_id: &str, prompt: &str, ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        if agent_id == ctx.agent {
            return Err(ToolFailure::Failed(format!(
                "delegate: an agent cannot call itself (`{agent_id}`)"
            )));
        }

        // Depth check (max recursion, from the parent frame).
        let parent_frame = self
            .store
            .get_frame(ctx.frame)
            .await
            .map_err(|e| ToolFailure::Failed(format!("delegate: frame lookup failed: {e}")))?
            .ok_or_else(|| ToolFailure::Failed("delegate: parent frame not found".into()))?;
        let new_depth = parent_frame.spec.depth + 1;
        if new_depth > self.max_depth {
            return Err(ToolFailure::Failed(format!(
                "delegate: maximum agent depth ({}) exceeded — refusing to recurse further",
                self.max_depth
            )));
        }

        let child_frame = self
            .store
            .open_frame(&ctx.conversation, Some(ctx.frame), FrameSpec {
                agent: agent_id.to_string(),
                prompt: Some(prompt.to_string()),
                depth: new_depth,
                parent_call: Some(ctx.call_id),
                meta: Value::Null,
            })
            .await
            .map_err(|e| ToolFailure::Failed(format!("delegate: open frame failed: {e}")))?;

        // Profile AFTER the frame exists (frame-scoped pieces anchor to it).
        // On rejection the frame is closed so nothing dangles.
        let profile = match self.catalog.get(agent_id, child_frame, ctx).await {
            Ok(p) => p,
            Err(e) => {
                let _ = self.store.close_frame(child_frame).await;
                return Err(ToolFailure::Failed(format!("delegate: {e}")));
            }
        };
        if profile.kind != AgentKind::Task {
            let _ = self.store.close_frame(child_frame).await;
            return Err(ToolFailure::Failed(format!(
                "delegate: agent `{agent_id}` is not dispatchable (only task agents are)"
            )));
        }

        self.store
            .append(child_frame, NewMessage::agent(prompt))
            .await
            .map_err(|e| ToolFailure::Failed(format!("delegate: append failed: {e}")))?;

        let events = EventSink::from_extensions(&ctx.extensions);
        if let Some(ev) = &events {
            ev.emit(child_frame, Some(ctx.frame), LoopEvent::AgentSpawned {
                frame: child_frame,
                agent: agent_id.to_string(),
                depth: new_depth,
                prompt_preview: preview_truncate(prompt, 500),
                parent_call: ctx.call_id,
                parent_agent: ctx.agent.clone(),
            });
        }

        // The child's tool set: the profile's full override, or the parent's
        // filtered per its ToolSelection.
        let child_tools: Arc<dyn ToolSet> = match profile.toolset.clone() {
            Some(ts) => ts,
            None => {
                let parent_tools = ctx
                    .extensions
                    .get::<SharedToolSet>()
                    .ok_or_else(|| ToolFailure::Failed("delegate: no ToolSet in extensions".into()))?;
                Arc::new(FilteredToolSet::derive(parent_tools.0.clone(), &profile.tools))
            }
        };

        let child = self
            .manager
            .start_loop(LoopParams {
                conversation: ctx.conversation.clone(),
                frame: child_frame,
                parent_frame: Some(ctx.frame),
                agent: agent_id.to_string(),
                system: profile.context,
                tools: child_tools,
                model_hint: profile.model.unwrap_or_default(),
                selector: profile.selector,
                // Sticky /stop: the child rides the parent's cancellation tree.
                token: Some(ctx.cancel.child_token()),
                live_input: None,
                extensions: ctx.extensions.clone(),
                meta: TurnMeta::default(),
                assembler: profile.assembler,
            })
            .await
            .map_err(|e| ToolFailure::Failed(format!("delegate: start loop failed: {e}")))?;

        let outcome = child.join().await;

        self.catalog.on_child_closed(child_frame).await;
        let _ = self.store.close_frame(child_frame).await;

        let result_preview = |s: &str| preview_truncate(s, 500);
        let emit_done = |text: &str| {
            if let Some(ev) = &events {
                ev.emit(child_frame, Some(ctx.frame), LoopEvent::AgentFinished {
                    frame: child_frame,
                    agent: agent_id.to_string(),
                    result_preview: result_preview(text),
                    parent_agent: ctx.agent.clone(),
                });
            }
        };

        match outcome {
            Ok(crate::kernel::TurnOutcome::Final { content, .. }) => {
                emit_done(&content);
                Ok(ToolOutput::Text(content))
            }
            Ok(crate::kernel::TurnOutcome::Cancelled) => {
                emit_done("⚠️ Cancelled.");
                Ok(ToolOutput::Text(format!("Sub-agent `{agent_id}` was cancelled.")))
            }
            Ok(crate::kernel::TurnOutcome::Exhausted) => {
                emit_done("⚠️ Exhausted tool-call rounds.");
                Ok(ToolOutput::Text(format!(
                    "Sub-agent `{agent_id}` exceeded the tool-call round budget without producing a final answer."
                )))
            }
            Err(e) => {
                emit_done(&format!("⚠️ Error: {e}"));
                Err(ToolFailure::Failed(format!("Sub-agent `{agent_id}` failed: {e}")))
            }
        }
    }
}

#[async_trait]
impl Tool for DelegateTool {
    fn name(&self) -> &str { &self.name }

    fn definition(&self) -> Value {
        if let Some(def) = &self.definition_override {
            return def.clone();
        }
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": "Delegate a task to a sub-agent and wait for its result. \
                                Use for focused, well-scoped work that benefits from a clean context.",
                "parameters": self.schema(),
            }
        })
    }

    /// Sync delegates batch: a homogeneous fan-out runs them concurrently
    /// (the kernel allocates ids in order first — results never mix).
    fn concurrency_safe(&self, args: &Value) -> bool {
        args["mode"].as_str() != Some("async")
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        let agent_id = args["agent_id"]
            .as_str()
            .ok_or_else(|| ToolFailure::Failed("delegate: missing required argument `agent_id`".into()))?;
        let prompt = args["prompt"]
            .as_str()
            .ok_or_else(|| ToolFailure::Failed("delegate: missing required argument `prompt`".into()))?;

        match args["mode"].as_str() {
            Some("async") => self.run_async(agent_id, prompt, &args, ctx).await,
            _             => self.run_sync(agent_id, prompt, ctx).await,
        }
    }
}

/// Truncate to `max` chars with an ellipsis (previews).
pub fn preview_truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// A static catalog for tests and simple hosts.
pub struct StaticCatalog {
    profiles: Vec<AgentProfile>,
}

impl StaticCatalog {
    pub fn new() -> Self { Self { profiles: Vec::new() } }

    pub fn with(mut self, profile: AgentProfile) -> Self {
        self.profiles.push(profile);
        self
    }
}

impl Default for StaticCatalog {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl AgentCatalog for StaticCatalog {
    async fn get(
        &self,
        id:           &str,
        _child_frame: FrameId,
        _ctx:         &ToolCtx,
    ) -> crate::Result<AgentProfile> {
        self.profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown agent `{id}`"))
    }

    async fn list(&self, kind: AgentKind) -> Vec<AgentSummary> {
        self.profiles
            .iter()
            .filter(|p| p.kind == kind)
            .map(|p| AgentSummary { id: p.id.clone(), kind: p.kind, description: String::new() })
            .collect()
    }
}
