use std::sync::Arc;

use async_trait::async_trait;

use crate::tool::Tool;

/// Pluggable long-term memory backend.
///
/// Implementations are registered with `MemoryManager` in the main crate.
/// At most one backend is active at a time (singleton rule enforced by the manager).
#[async_trait]
pub trait Memory: Send + Sync {
    /// Unique identifier for this backend (e.g. `"honcho"`).
    fn id(&self) -> &str;

    /// Returns `true` when the backend is reachable and ready.
    fn is_available(&self) -> bool;

    /// Retrieves context for the upcoming turn to inject into the system prompt.
    /// Returns `None` on cold start, backend down, or nothing useful available.
    async fn query_context(&self, session_id: i64, user_message: &str) -> Option<String>;

    /// Optional LLM-callable tools exposed by this backend (e.g. `memory_query`).
    /// Called per turn — added to the live tool list and dispatched before the
    /// global tool registry.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![]
    }
}
