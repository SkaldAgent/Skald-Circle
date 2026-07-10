use async_trait::async_trait;

/// Minimal approval API exposed to plugins.
///
/// Plugins receive `Arc<dyn ApprovalApi>` via `PluginContext` and use it to
/// resolve pending tool-call approvals without depending on the main crate's
/// `ApprovalManager` directly.
#[async_trait]
pub trait ApprovalApi: Send + Sync {
    /// Approve a pending tool-call request.
    async fn approve(&self, request_id: i64);

    /// Reject a pending tool-call request with an optional note.
    async fn reject(&self, request_id: i64, note: String);

    /// Approve + register a session bypass so future tool calls of the same
    /// category/MCP-server are skipped automatically.
    ///
    /// - `bypass_secs = Some(n)`: bypass lasts `n` seconds (e.g. 900 = 15 min)
    /// - `bypass_secs = None`: bypass lasts until the session ends
    async fn approve_with_bypass(&self, request_id: i64, bypass_secs: Option<u64>);
}
