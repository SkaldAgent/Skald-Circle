//! Memory abstraction layer.
//!
//! Provides a [`Memory`] trait for pluggable long-term memory backends, and a
//! [`MemoryManager`] that holds at most **one** active backend at a time.
//!
//! # Singleton rule
//! Only one backend can be registered. If a second backend (with a different id)
//! tries to register, it is rejected with an `error!` log and the first one is
//! kept. The same backend can re-register itself (e.g. after a config change /
//! restart) — that replaces the existing registration cleanly.
//!
//! # Integration points
//! - [`Memory::query_context`] is called at the start of every `handle_message`
//!   turn. The returned string is prepended to `extra_system_context` and
//!   injected into the system prompt.
//! - [`Memory::tools`] is called per turn; the returned tools are added to the
//!   LLM's tool list and dispatched before the global registry.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::RwLock;
use tracing::{error, info};

pub use core_api::memory::Memory;

use crate::tools::Tool;

// ── MemoryManager ─────────────────────────────────────────────────────────────

pub struct MemoryManager {
    backend: RwLock<Option<Arc<dyn Memory>>>,
}

impl MemoryManager {
    pub fn new() -> Self {
        Self { backend: RwLock::new(None) }
    }

    /// Registers a memory backend.
    ///
    /// - If no backend is registered yet, the new one is accepted.
    /// - If the same backend id re-registers (restart / config change), it replaces
    ///   the old entry.
    /// - If a **different** backend id tries to register while one is already active,
    ///   it is rejected with `error!` and the existing backend is kept.
    pub async fn register(&self, backend: Arc<dyn Memory>) {
        let mut lock = self.backend.write().await;
        match lock.as_ref() {
            None => {
                info!("MemoryManager: registered backend '{}'", backend.id());
                *lock = Some(backend);
            }
            Some(existing) if existing.id() == backend.id() => {
                info!("MemoryManager: replacing backend '{}' (restart/reload)", backend.id());
                *lock = Some(backend);
            }
            Some(existing) => {
                error!(
                    "MemoryManager: backend '{}' is already registered — \
                     discarding '{}'. Only one memory backend is supported at a time.",
                    existing.id(),
                    backend.id(),
                );
            }
        }
    }

    /// Returns memory context to inject into the system prompt for the upcoming
    /// turn. Returns `None` if no backend is registered or the backend is
    /// unavailable / has nothing to say.
    pub async fn query_context(&self, session_id: i64, user_message: &str) -> Option<String> {
        let backend = self.backend.read().await.clone()?;
        if !backend.is_available() {
            return None;
        }
        backend.query_context(session_id, user_message).await
    }

    /// Returns the per-turn LLM tools exposed by the active backend.
    /// Empty if no backend is registered or the backend is unavailable.
    pub async fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let backend = self.backend.read().await.clone();
        match backend {
            Some(b) if b.is_available() => b.tools(),
            _ => vec![],
        }
    }

    /// Builds OpenAI-format tool definitions from the active backend's tools.
    pub async fn tool_defs(&self) -> Vec<Value> {
        self.tools().await
            .iter()
            .map(|t| t.openai_definition())
            .collect()
    }
}

impl Default for MemoryManager {
    fn default() -> Self { Self::new() }
}
