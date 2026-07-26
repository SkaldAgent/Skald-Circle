//! `agent-loop` — a reusable LLM agent-loop kernel.
//!
//! The crate owns the **control flow** of a tool-calling agent loop (round loop,
//! model fallback, parallel tool fan-out, streaming deltas, cancellation) and the
//! **LLM clients + protocols** (OpenAI-compatible, Anthropic, Ollama, LM Studio;
//! SSE; dynamic tool loading wire semantics). It knows nothing about databases,
//! agents, MCP, approval rules or Docker: the host implements the trait surface
//! (`Model`, `ModelSelector`, `HistoryStore`, `ContextAssembler`,
//! `SystemContextSource`, `Tool`, `ToolSet`, `Gate`, `LoopHooks`, `HumanChannel`,
//! `ActivationSource`, `ToolActivator`) or uses the shipped defaults.
//!
//! Design document: `blueprint/project-loop.md` (Skald workspace).

pub mod activation;
pub mod compaction;
pub mod context;
pub mod delegate;
pub mod events;
pub mod gate;
pub mod hooks;
pub mod human;
pub mod ids;
pub mod kernel;
pub mod manager;
pub mod model;
pub mod models;
pub mod projection;
pub mod recovery;
pub mod store;
pub mod store_memory;
pub mod testing;
pub mod tool;

/// Re-exported so implementors of the crate's async traits can write
/// `#[agent_loop::async_trait]` without a direct dependency.
pub use async_trait::async_trait;

/// Application name sent as the `X-Title` header by the shipped clients
/// (OpenRouter rankings). Clients accept an override.
pub const APP_NAME: &str = "Skald";

/// Crate-wide result type for host-implemented traits.
pub type Result<T> = anyhow::Result<T>;

pub mod prelude {
    pub use crate::activation::{
        ActivateToolsTool, Activation, ActivationSource, ToolActivator, ToolRendering,
    };
    pub use crate::compaction::{
        Compaction, CompactionMode, CompactionOutcome, CompactionPrompt, should_compact,
    };
    pub use crate::context::{
        AssembleInput, ContextAssembler, LinearAssembler, StaticSystemContext, SystemContext,
        SystemContextSource, TurnInfo,
    };
    pub use crate::delegate::{
        AgentCatalog, AgentKind, AgentProfile, AgentSummary, AsyncExecutor, AsyncResultSink,
        AsyncSpec, CompletedTask, DelegateTool, FilteredToolSet, InProcessExecutor, StaticCatalog,
        StoreSink, TaskHandle, ToolSelection,
    };
    pub use crate::events::{DeltaKind, Event, EventSink, LoopEvent};
    pub use crate::gate::{AllowAll, DenyList, Gate, GateDecision, PendingCall};
    pub use crate::hooks::{HookCtx, HookVerdict, LoopHooks};
    pub use crate::human::{AskUserTool, HumanChannel, HumanGone, Question};
    pub use crate::ids::{
        ConversationId, FrameId, MessageId, ModelId, SummaryId, TaskId, ToolCallId,
    };
    pub use crate::manager::{
        LiveInput, LoopManager, LoopManagerBuilder, LoopParams, StartError, TurnHandle, TurnMeta,
        TurnParams,
    };
    pub use crate::model::{
        Model, ModelError, ModelHandle, ModelHint, ModelInfo, ModelRequest, ModelResponse,
        ModelSelector, RawMeta, RetryPolicy, SingleModel, StaticModels, StreamDelta, ToolCall,
        Usage,
    };
    pub use crate::recovery::{
        HumanDecision, PendingPolicy, Recovery, RecoveryPolicy, RecoveryReport, RunningPolicy,
    };
    pub use crate::projection::{
        MediaBlob, MediaBudget, MediaKind, MediaSource, Projection, ProjectionHooks,
        ReasoningEcho, ResultLimit, ToolResultDigest,
    };
    pub use crate::store::{
        CallOutcome, CallState, FrameRecord, FrameSpec, HistoryStore, NewCall, NewMessage,
        NewSummary, Role, StoredCall, StoredMessage, StoredSummary,
    };
    pub use crate::tool::{
        Extensions, MediaRef, RestartHint, SimpleExecution, Tool, ToolCtx, ToolExecution,
        ToolFailure, ToolOutput, ToolSet, Visibility, drive_execution,
    };
    pub use crate::{APP_NAME, Result};
    pub use async_trait::async_trait;
    pub use serde_json::{Value, json};
    pub use tokio_util::sync::CancellationToken;
}
