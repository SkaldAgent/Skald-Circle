//! `LoopManager` — the singleton (per tenant/user) that owns the event bus and
//! the registry of live loops, and spawns disposable `LlmLoop`s (blueprint D1).
//!
//! Policy: **one live loop per conversation** — `start_turn` rejects a second
//! one (anti double-driving). Serialization/queueing of user messages stays
//! with the host.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::context::{ContextAssembler, LinearAssembler, SystemContextSource};
use crate::events::{Event, EventSink, LoopEvent};
use crate::gate::{AllowAll, Gate};
use crate::hooks::LoopHooks;
use crate::human::HumanChannel;
use crate::ids::{ConversationId, FrameId};
use crate::kernel::{KernelDeps, TurnOutcome};
use crate::model::{ModelHint, ModelSelector, RetryPolicy};
use crate::store::{FrameSpec, HistoryStore, NewMessage, Role};
use crate::tool::{Extensions, ToolSet};

// ── LiveInput ────────────────────────────────────────────────────────────────

/// Pull-based live user input (blueprint D10): drained at round boundaries.
#[async_trait]
pub trait LiveInput: Send + Sync {
    async fn drain(&self) -> Vec<NewMessage>;
}

// ── TurnMeta ─────────────────────────────────────────────────────────────────

/// Per-turn metadata.
#[derive(Debug, Clone, Default)]
pub struct TurnMeta {
    /// Synthetic turn (TIC/notify) — no user echo semantics.
    pub synthetic:     bool,
    /// Interactive surface (web chat, telegram, …).
    pub interactive:   bool,
    /// Label for UI/logging ("session 42", "cron job X").
    pub context_label: Option<String>,
    /// The user message that opened the turn (for `TurnInfo`).
    pub user_message:  Option<String>,
}

// ── TurnParams / LoopParams ──────────────────────────────────────────────────

/// Parameters of a user turn (root frame).
pub struct TurnParams {
    /// Root frame (opened by the host or via `LoopManager::open_root`).
    pub frame:      FrameId,
    pub agent:      String,
    pub system:     Arc<dyn SystemContextSource>,
    /// Already filtered (visibility/approval).
    pub tools:      Arc<dyn ToolSet>,
    pub model_hint: ModelHint,
    /// None for sub-agents / cron / resume.
    pub live_input: Option<Arc<dyn LiveInput>>,
    /// Flows into `ToolCtx.extensions`.
    pub extensions: Extensions,
    pub meta:       TurnMeta,
    /// Per-turn assembler override (default: the manager's).
    pub assembler:  Option<Arc<dyn ContextAssembler>>,
}

/// Parameters of a raw loop (DelegateTool, recovery, background runners).
pub struct LoopParams {
    pub conversation: ConversationId,
    pub frame:        FrameId,
    pub parent_frame: Option<FrameId>,
    pub agent:        String,
    pub system:       Arc<dyn SystemContextSource>,
    pub tools:        Arc<dyn ToolSet>,
    pub model_hint:   ModelHint,
    pub live_input:   Option<Arc<dyn LiveInput>>,
    pub extensions:   Extensions,
    pub meta:         TurnMeta,
    pub assembler:    Option<Arc<dyn ContextAssembler>>,
}

// ── TurnHandle ───────────────────────────────────────────────────────────────

/// Handle of a spawned turn.
pub struct TurnHandle {
    pub conversation: ConversationId,
    pub frame:        FrameId,
    /// Clone; cancels THIS turn (sticky down the whole call tree).
    pub cancel:       CancellationToken,
    join:             JoinHandle<crate::Result<TurnOutcome>>,
}

impl TurnHandle {
    pub async fn join(self) -> crate::Result<TurnOutcome> {
        self.join.await.map_err(|e| anyhow::anyhow!("loop task panicked: {e}"))?
    }
}

// ── StartError ───────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum StartError {
    /// A loop is already live on this conversation (anti double-driving).
    AlreadyRunning,
    Store(anyhow::Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning => write!(f, "a loop is already running on this conversation"),
            Self::Store(e)       => write!(f, "store error: {e}"),
        }
    }
}
impl std::error::Error for StartError {}

// ── RunningInfo ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RunningInfo {
    pub conversation: ConversationId,
    pub frame:        FrameId,
    pub agent:        String,
}

struct RunningEntry {
    frame:  FrameId,
    agent:  String,
    cancel: CancellationToken,
}

// ── LoopManager ──────────────────────────────────────────────────────────────

pub struct LoopManager {
    deps:     Arc<KernelDeps>,
    bus:      broadcast::Sender<Event<LoopEvent>>,
    registry: Arc<Mutex<HashMap<ConversationId, RunningEntry>>>,
    human:    Option<Arc<dyn HumanChannel>>,
}

impl LoopManager {
    pub fn builder() -> LoopManagerBuilder { LoopManagerBuilder::default() }

    /// Subscribe to the global event bus (every event tagged with
    /// conversation/frame/parent_frame).
    pub fn events(&self) -> broadcast::Receiver<Event<LoopEvent>> { self.bus.subscribe() }

    /// The host-provided human channel, if any.
    pub fn human(&self) -> Option<Arc<dyn HumanChannel>> { self.human.clone() }

    /// Convenience: open a root frame on the store.
    pub async fn open_root(&self, conv: &ConversationId, spec: FrameSpec) -> crate::Result<FrameId> {
        self.deps.store.open_frame(conv, None, spec).await
    }

    pub fn store(&self) -> Arc<dyn HistoryStore> { self.deps.store.clone() }

    // ── user turns ──

    /// High-level entry point:
    /// 1. rejects when a loop is already live on the conversation;
    /// 2. marks a trailing orphan User/Agent message failed (alternation rule
    ///    for strict APIs);
    /// 3. appends the user message + echo event;
    /// 4. spawns the loop; returns the handle immediately.
    pub async fn start_turn(
        &self,
        conv:    ConversationId,
        msg:     NewMessage,
        mut params: TurnParams,
    ) -> Result<TurnHandle, StartError> {
        {
            let registry = self.registry.lock().unwrap();
            if registry.contains_key(&conv) {
                return Err(StartError::AlreadyRunning);
            }
        }

        // Orphan rule: a trailing User/Agent message with no assistant reply
        // breaks strict alternation — mark it failed before appending.
        if let Some(last) = self.deps.store.last(params.frame).await.map_err(StartError::Store)?
            && matches!(last.role, Role::User | Role::Agent)
        {
            self.deps.store.mark_failed(last.id).await.map_err(StartError::Store)?;
        }

        let events = self.sink(conv.clone());
        let id = self.deps.store.append(params.frame, msg.clone()).await.map_err(StartError::Store)?;
        events.emit(params.frame, None, LoopEvent::UserMessage {
            message_id: id,
            content: msg.content.clone(),
            synthetic: msg.synthetic,
            metadata: msg.metadata.clone(),
        });

        params.meta.user_message = Some(msg.content);
        self.spawn(LoopParams {
            conversation: conv,
            frame: params.frame,
            parent_frame: None,
            agent: params.agent,
            system: params.system,
            tools: params.tools,
            model_hint: params.model_hint,
            live_input: params.live_input,
            extensions: params.extensions,
            meta: params.meta,
            assembler: params.assembler,
        })
    }

    // ── raw loops (DelegateTool, recovery, background runners) ──

    pub async fn start_loop(&self, params: LoopParams) -> Result<TurnHandle, StartError> {
        {
            let registry = self.registry.lock().unwrap();
            if registry.contains_key(&params.conversation) {
                return Err(StartError::AlreadyRunning);
            }
        }
        self.spawn(params)
    }

    fn spawn(&self, params: LoopParams) -> Result<TurnHandle, StartError> {
        let conv = params.conversation.clone();
        let frame = params.frame;
        let agent = params.agent.clone();
        let token = CancellationToken::new();
        let events = self.sink(conv.clone());

        {
            let mut registry = self.registry.lock().unwrap();
            registry.insert(conv.clone(), RunningEntry {
                frame,
                agent,
                cancel: token.clone(),
            });
        }

        let deps = self.deps.clone();
        let registry = self.registry.clone();
        let turn_token = token.clone();
        let join_conv = conv.clone();
        let join = tokio::spawn(async move {
            let outcome = crate::kernel::run(deps, params, turn_token, events).await;
            registry.lock().unwrap().remove(&join_conv);
            outcome
        });

        Ok(TurnHandle { conversation: conv, frame, cancel: token, join })
    }

    // ── control ──

    /// `/stop`: cancel the live loop on a conversation, if any.
    pub fn cancel(&self, conv: &ConversationId) {
        if let Some(entry) = self.registry.lock().unwrap().get(conv) {
            entry.cancel.cancel();
        }
    }

    pub fn is_running(&self, conv: &ConversationId) -> bool {
        self.registry.lock().unwrap().contains_key(conv)
    }

    /// Global view (UI "running agents").
    pub fn list_running(&self) -> Vec<RunningInfo> {
        self.registry
            .lock()
            .unwrap()
            .iter()
            .map(|(conversation, e)| RunningInfo {
                conversation: conversation.clone(),
                frame: e.frame,
                agent: e.agent.clone(),
            })
            .collect()
    }

    /// Cancel all live loops. Joins are detached — callers wanting a drain
    /// should hold the handles.
    pub async fn shutdown(&self) {
        let tokens: Vec<CancellationToken> = self
            .registry
            .lock()
            .unwrap()
            .values()
            .map(|e| e.cancel.clone())
            .collect();
        for t in tokens {
            t.cancel();
        }
    }

    fn sink(&self, conv: ConversationId) -> EventSink {
        EventSink::new(conv, self.bus.clone())
    }
}

// ── Builder ──────────────────────────────────────────────────────────────────

pub struct LoopManagerBuilder {
    models:             Option<Arc<dyn ModelSelector>>,
    store:              Option<Arc<dyn HistoryStore>>,
    gate:               Option<Arc<dyn Gate>>,
    hooks:              Vec<Arc<dyn LoopHooks>>,
    human:              Option<Arc<dyn HumanChannel>>,
    assembler:          Option<Arc<dyn ContextAssembler>>,
    max_rounds:         usize,
    max_parallel_calls: usize,
    retry:              RetryPolicy,
    bus_capacity:       usize,
}

impl Default for LoopManagerBuilder {
    fn default() -> Self {
        Self {
            models: None,
            store: None,
            gate: None,
            hooks: Vec::new(),
            human: None,
            assembler: None,
            max_rounds: 20,
            max_parallel_calls: 4,
            retry: RetryPolicy::default(),
            bus_capacity: 512,
        }
    }
}

impl LoopManagerBuilder {
    pub fn models(mut self, models: Arc<dyn ModelSelector>) -> Self {
        self.models = Some(models);
        self
    }

    pub fn store(mut self, store: Arc<dyn HistoryStore>) -> Self {
        self.store = Some(store);
        self
    }

    pub fn gate(mut self, gate: impl Gate + 'static) -> Self {
        self.gate = Some(Arc::new(gate));
        self
    }

    pub fn gate_arc(mut self, gate: Arc<dyn Gate>) -> Self {
        self.gate = Some(gate);
        self
    }

    pub fn hook(mut self, hook: Arc<dyn LoopHooks>) -> Self {
        self.hooks.push(hook);
        self
    }

    pub fn human(mut self, human: Arc<dyn HumanChannel>) -> Self {
        self.human = Some(human);
        self
    }

    pub fn assembler(mut self, assembler: Arc<dyn ContextAssembler>) -> Self {
        self.assembler = Some(assembler);
        self
    }

    pub fn max_rounds(mut self, n: usize) -> Self {
        self.max_rounds = n;
        self
    }

    pub fn max_parallel_calls(mut self, n: usize) -> Self {
        self.max_parallel_calls = n;
        self
    }

    pub fn retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn bus_capacity(mut self, n: usize) -> Self {
        self.bus_capacity = n;
        self
    }

    pub fn build(self) -> crate::Result<LoopManager> {
        let deps = Arc::new(KernelDeps {
            models: self.models.ok_or_else(|| anyhow::anyhow!("LoopManager: models required"))?,
            store: self.store.ok_or_else(|| anyhow::anyhow!("LoopManager: store required"))?,
            gate: self.gate.unwrap_or_else(|| Arc::new(AllowAll)),
            hooks: self.hooks,
            assembler: self.assembler.unwrap_or_else(|| Arc::new(LinearAssembler::new())),
            max_rounds: self.max_rounds,
            max_parallel_calls: self.max_parallel_calls,
            retry: self.retry,
        });
        let (bus, _) = broadcast::channel(self.bus_capacity);
        Ok(LoopManager {
            deps,
            bus,
            registry: Arc::new(Mutex::new(HashMap::new())),
            human: self.human,
        })
    }
}
