use axum::Json;
use axum::extract::State;
use serde_json::Value;

use std::sync::Arc;
use skald_core::skald::Skald;

/// Returns the list of running MCP servers and their available tools.
pub async fn list_servers(State(skald): State<Arc<Skald>>) -> Json<Vec<Value>> {
    Json(skald.mcp().server_infos())
}
