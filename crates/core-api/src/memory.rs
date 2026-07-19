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
    ///
    /// `user_id` is the session owner: multi-user backends scope retrieval to that
    /// user's own memory (e.g. their Honcho peer), and `session_id` is local to
    /// that user's pool so it must be namespaced by `user_id` to stay unique.
    async fn query_context(&self, user_id: &str, session_id: i64, user_message: &str) -> Option<String>;

    /// Optional LLM-callable tools exposed by this backend (e.g. `memory_query`).
    /// Called per turn — added to the live tool list and dispatched before the
    /// global tool registry.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![]
    }
}
