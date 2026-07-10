use std::collections::HashMap;

use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct McpServerConfig {
    pub name:      String,
    pub transport: McpTransport,
    /// stdio only: the executable to spawn.
    pub command:   Option<String>,
    /// stdio only: arguments passed to the command.
    pub args:      Option<Vec<String>>,
    /// stdio only: extra environment variables (values support `${VAR}` interpolation).
    pub env:       Option<HashMap<String, String>>,
    /// http only: base URL of the MCP server.
    pub url:     Option<String>,
    /// http only: API key sent as `Authorization: Bearer <key>` (supports `${VAR}` interpolation).
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    /// Streamable HTTP (remote MCP servers — previously called SSE).
    Http,
    /// Alias kept for backwards compatibility.
    Sse,
}
