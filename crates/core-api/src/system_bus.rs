//! In-process event bus for system-level lifecycle events.
//!
//! Distinct from [`crate::bus::ChatEventBus`] which carries chat-turn events.
//! This bus carries infrastructure events: provider registration, plugin state
//! changes, etc.  Any component can subscribe and react without direct coupling.
//!
//! # Usage
//! ```rust,ignore
//! // Producer (e.g. a plugin):
//! bus.send(SystemEvent::ApiProviderRegistered { type_id: "elevenlabs".into() });
//!
//! // Consumer (spawn once at startup):
//! let mut rx = bus.subscribe();
//! tokio::spawn(async move {
//!     loop {
//!         match rx.recv().await {
//!             Ok(SystemEvent::ApiProviderRegistered { type_id }) => { /* reload */ }
//!             Ok(SystemEvent::ApiProviderUnregistered { type_id }) => { /* reload */ }
//!             Err(RecvError::Lagged(n)) => warn!("system_bus lagged by {n}"),
//!             Err(RecvError::Closed)    => break,
//!         }
//!     }
//! });
//! ```

use tokio::sync::broadcast;

pub use tokio::sync::broadcast::error::RecvError;

const DEFAULT_CAPACITY: usize = 64;

// ── Events ────────────────────────────────────────────────────────────────────

/// All system-level events that flow through the [`SystemEventBus`].
#[derive(Debug, Clone)]
pub enum SystemEvent {
    /// A plugin registered a new `ApiProvider` (e.g. ElevenLabs on plugin start).
    ApiProviderRegistered { type_id: String },
    /// A plugin unregistered an `ApiProvider` (e.g. on plugin stop/disable).
    ApiProviderUnregistered { type_id: String },
    /// A config key was changed via the API (only fires when the value actually changes).
    ConfigKeyUpdated { key: String, old_value: Option<String>, new_value: String },
    /// A scheduled job finished (success or failure). `origin_ref` is the opaque
    /// string stored in `scheduled_jobs.origin_ref` (e.g. `"PROJECT_TASK:42"`).
    JobCompleted {
        job_id:     i64,
        origin_ref: Option<String>,
        result:     Option<String>,
        error:      Option<String>,
    },
    /// A session was forcibly cancelled (e.g. via the kill-task API).
    /// Subscribers should cancel any in-flight LLM turn, pending approvals,
    /// and pending clarifications for that session.
    SessionCancelled {
        session_id: i64,
    },

    // ── User lifecycle (blueprint §6) ─────────────────────────────────────────
    // Announced by whoever changed the row; the reaction — provisioning, tearing
    // down or remounting a Docker container — belongs to the lifecycle reconciler
    // in `skald-core`, never to the endpoint that made the change.
    /// A user was created, by any creator (the Users admin page, the first-run
    /// setup wizard). Their execution sandbox has to be provisioned.
    UserCreated {
        user_id: String,
    },
    /// A user was deleted. Their sandbox has to be torn down.
    UserDeleted {
        user_id: String,
    },
    /// A user was deactivated (`false`) or reactivated (`true`). Their sandbox is
    /// stopped or started to match — boot reconciliation keeps a container only for
    /// *active* users, so this is the running-server equivalent.
    ///
    /// Revoking the live runtime (sessions, loops, database key) is **not** on this
    /// event: it is an authorization invariant and runs synchronously in the handler
    /// (`Skald::revoke_user_runtime`), because a lossy broadcast is the wrong
    /// transport for "this person must stop being logged in".
    UserActiveChanged {
        user_id: String,
        active:  bool,
    },
    /// A user's **mount topology** changed — a shared-folder or project membership
    /// was granted, revoked or re-graded (RO ⇄ RW). Their container must be
    /// recreated against the new mount set, and a live session's filesystem view
    /// refreshed with it.
    UserMountsChanged {
        user_id: String,
    },

    // ── Connectors (blueprint §7) ─────────────────────────────────────────────
    /// The set of **global** MCP connectors changed — one was enabled (and started)
    /// or deleted (and stopped). Every live user re-snapshots their access filter so
    /// the connector appears in / disappears from `MCP_LIST` without a re-login.
    ///
    /// Emitted only for changes to the *server set*. Changing **who may use** a
    /// connector is a grant/revoke and stays synchronous in its handler, for the same
    /// reason as [`Self::UserActiveChanged`]: this bus promises "eventually", which is
    /// the wrong promise for taking access away.
    McpGlobalServersChanged,
    /// A marketplace connector was (re)installed. Anything already running it — the
    /// global runtime, each live user's per-user runtime — re-reads its metadata and
    /// re-copies its files/deps, so the new version lands without a re-login.
    ConnectorReinstalled {
        catalog_name: String,
    },

    // ── Reports (blueprint §13) ───────────────────────────────────────────────
    /// A background agent filed a report. Announced by whoever wrote the row,
    /// never delivered by it: *who* should hear about a report — the people
    /// supervising its subject, an unread badge, a future digest — is a question
    /// the producer has no business answering, and answering it there would make
    /// every new recipient a change to every agent that writes one.
    ///
    /// Best-effort like everything on this bus, which is the right promise here: a
    /// missed announcement costs a notification, not the report, and the row is
    /// already durable by the time this is sent. `subject_user_id` is `None` for a
    /// report about nobody in particular.
    ReportCreated {
        report_id:       i64,
        kind:            String,
        subject_user_id: Option<String>,
    },
}

// ── Bus ───────────────────────────────────────────────────────────────────────

pub struct SystemEventBus {
    tx: broadcast::Sender<SystemEvent>,
}

impl SystemEventBus {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish an event. No-op if there are no active subscribers.
    pub fn send(&self, event: SystemEvent) {
        let _ = self.tx.send(event);
    }

    /// Returns a new independent receiver. Each subscriber gets every future
    /// event independently. If the subscriber falls behind by more than the
    /// channel capacity it receives `RecvError::Lagged(n)`.
    pub fn subscribe(&self) -> broadcast::Receiver<SystemEvent> {
        self.tx.subscribe()
    }
}

impl Default for SystemEventBus {
    fn default() -> Self {
        Self::new()
    }
}
