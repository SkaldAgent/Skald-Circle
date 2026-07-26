//! `TurnScope` — everything about the turn in flight, published once in the
//! kernel's `Extensions`.
//!
//! The adapters that need it (the approval gate, the agent catalog) live as
//! long as the **user**, not the turn: one `LoopManager` per `UserContext`
//! (blueprint D12) means they cannot capture a session id, a source or a
//! permission group at construction. So they read them from here — the seam the
//! library designed for exactly this (`PendingCall.extensions`,
//! `ToolCtx.extensions`, blueprint §4.6).
//!
//! Everything mutable rides a shared cell, so a change during the turn (a
//! `/stop`-time auto-deny flip, a security-group switch, an `activate_tools`
//! grant) is seen by the adapters without rebuilding anything.

use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};

use serde_json::Value;
use tokio::sync::RwLock as AsyncRwLock;

use crate::run_context::RunContext;
use crate::tools::Tool;

/// The turn's own state. Cheap to build (everything is an `Arc` or a small
/// value) because it is built once per turn.
pub struct TurnScope {
    // ── identity ──
    pub session_id:     i64,
    pub source:         String,
    pub is_interactive: bool,
    pub agent_id:       String,
    /// Scratchpad scope: the session's own id, or the parent's for an async
    /// sub-task.
    pub scratchpad_sid: i64,
    /// Project root (agent path) when this is a project session.
    pub project_root:   Option<String>,

    // ── live cells (shared with the session handler) ──
    pub context_label:  Arc<RwLock<Option<String>>>,
    pub run_context:    Arc<AsyncRwLock<Option<RunContext>>>,
    /// Security group driving the approval rules.
    pub group_id:       Option<String>,
    /// Calls a human approved through a REST resolve after a restart: the gate
    /// lets them through once.
    pub pre_approved:   Arc<Mutex<HashSet<i64>>>,
    /// Surfaces that cannot ask a human deny instead of hanging.
    pub auto_deny:      Arc<AtomicBool>,
    /// MCP servers (plus the reserved `config` group) activated for this turn;
    /// `activate_tools` mutates it, and the next round sees the new tools.
    pub grants:         Arc<RwLock<HashSet<String>>>,

    // ── tool material a child agent derives its own set from ──
    pub base_defs:    Arc<Vec<Value>>,
    pub config_defs:  Arc<Vec<Value>>,
    pub memory_tools: Arc<Vec<Arc<dyn Tool>>>,
    pub image_tools:  Arc<Vec<Arc<dyn Tool>>>,
    pub root_only:    Arc<Vec<String>>,
}

impl TurnScope {
    /// The scope of the turn a call belongs to.
    ///
    /// Absence is a wiring bug, not a runtime condition — every turn publishes
    /// one — so callers fail closed (deny / refuse to delegate) rather than
    /// guessing a permissive default.
    pub fn from(extensions: &agent_loop::tool::Extensions) -> Option<Arc<Self>> {
        extensions.get::<TurnScope>()
    }
}
