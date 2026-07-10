use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

/// Future returned by an [`InterfaceTool`] handler.
pub type ToolFuture = Pin<Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send>>;

/// A single LLM-callable tool injected by a specific interface (Telegram, Web, Cron, …).
///
/// The handler closure captures interface-specific state (e.g. `Arc<Bot>` + `ChatId`).
pub struct InterfaceTool {
    /// OpenAI-format tool definition sent to the LLM in the tools array.
    pub definition: Value,
    /// Async handler invoked when the LLM calls this tool.
    pub handler: Arc<dyn Fn(Value) -> ToolFuture + Send + Sync>,
}

impl InterfaceTool {
    /// Returns the tool's name as declared in the definition.
    pub fn name(&self) -> &str {
        self.definition["function"]["name"].as_str().unwrap_or("")
    }
}
