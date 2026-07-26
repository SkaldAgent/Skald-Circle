//! Sub-agents as a tool (blueprint §7, D2): the kernel never intercepts
//! anything — `delegate` is a tool like any other, dispatched through the
//! normal gate/hooks/execution path. A sync child is just a slow tool call the
//! parent awaits; a homogeneous batch of sync delegates fans out through the
//! kernel's generic concurrency (`concurrency_safe`).
//!
//! The crate ships the SYNC flow. Async delegation rides the host's
//! `AsyncExecutor` (phase-3 concern: Skald wires its durable cron executor
//! there); calling it here fails with a clear error.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::async_trait;
use crate::context::SystemContextSource;
use crate::events::{EventSink, LoopEvent};
use crate::ids::FrameId;
use crate::manager::{LoopManager, LoopParams, TurnMeta};
use crate::model::{ModelHint, ModelSelector};
use crate::store::{FrameSpec, HistoryStore, NewMessage};
use crate::tool::{SharedToolSet, Tool, ToolCtx, ToolFailure, ToolOutput, ToolSet};

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
    async fn get(&self, id: &str, child_frame: FrameId) -> crate::Result<AgentProfile>;
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
}

impl DelegateTool {
    pub fn new(
        manager:   Arc<LoopManager>,
        catalog:   Arc<dyn AgentCatalog>,
        store:     Arc<dyn HistoryStore>,
        max_depth: u32,
    ) -> Self {
        Self { manager, catalog, store, max_depth, name: "delegate".to_string(), definition_override: None }
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
        let profile = match self.catalog.get(agent_id, child_frame).await {
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
                Arc::new(FilteredToolSet {
                    inner:  parent_tools.0.clone(),
                    remove: profile.tools.remove.clone(),
                    add:    profile.tools.add.clone(),
                })
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
            Some("async") => Err(ToolFailure::Failed(
                "delegate: async mode rides the host's AsyncExecutor, which is not wired on this path"
                    .to_string(),
            )),
            _ => self.run_sync(agent_id, prompt, ctx).await,
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
    async fn get(&self, id: &str, _child_frame: FrameId) -> crate::Result<AgentProfile> {
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
