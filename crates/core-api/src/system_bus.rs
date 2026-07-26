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
    /// A user's **mount topology** changed — a shared-folder or project membership
    /// was granted, revoked or re-graded (RO ⇄ RW). Their container must be
    /// recreated against the new mount set, and a live session's filesystem view
    /// refreshed with it.
    UserMountsChanged {
        user_id: String,
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
