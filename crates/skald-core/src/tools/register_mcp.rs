use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::db::mcp_servers::UpsertParams;
use crate::mcp::McpManager;
use crate::tools::{Tool, ToolDescriptionLength};

pub struct RegisterMcp {
    mcp: Arc<McpManager>,
}

impl RegisterMcp {
    pub fn new(mcp: Arc<McpManager>) -> Self { Self { mcp } }
}

impl Tool for RegisterMcp {
    fn name(&self) -> &str { "register_mcp" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Config }

    fn description(&self) -> &str {
        "Register (or update) an MCP server and connect to it immediately. \
         For stdio servers supply `command` and optionally `args` and `env`. \
         For HTTP/SSE servers supply `url` and optionally `api_key`. \
         Optionally provide `description` (what the server does) and `friendly_name` (display name for UI). \
         Returns the list of tools exposed by the server once connected."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Unique name for this MCP server (used to reference it in tool calls)."
                },
                "transport": {
                    "type": "string",
                    "enum": ["stdio", "http", "sse"],
                    "description": "Connection transport. Use `stdio` for local processes, `http` for remote servers."
                },
                "command": {
                    "type": "string",
                    "description": "stdio only: executable to spawn (e.g. `npx`, `uvx`, path to binary)."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "stdio only: command-line arguments passed to the executable."
                },
                "env": {
                    "type": "object",
                    "additionalProperties": { "type": "string" },
                    "description": "stdio only: extra environment variables. Values support `${VAR}` interpolation."
                },
                "url": {
                    "type": "string",
                    "description": "http/sse only: base URL of the remote MCP server."
                },
                "api_key": {
                    "type": "string",
                    "description": "http/sse only: API key sent as `Authorization: Bearer <key>`."
                },
                "description": {
                    "type": "string",
                    "description": "A short description of what this MCP server provides (shown in list_items type=mcp)."
                },
                "friendly_name": {
                    "type": "string",
                    "description": "A human-readable display name for this MCP server (e.g. 'Google Calendar')."
                }
            },
            "required": ["name", "transport"]
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let name = args["name"].as_str().unwrap_or("?");
        format!("register MCP `{name}`")
    }

    fn execute(&self, args: Value) -> Result<String> {
        let name = args["name"].as_str()
            .ok_or_else(|| anyhow::anyhow!("register_mcp: missing required argument `name`"))?;
        let transport = args["transport"].as_str()
            .ok_or_else(|| anyhow::anyhow!("register_mcp: missing required argument `transport`"))?;

        let args_json = args["args"].as_array()
            .map(|a| serde_json::to_string(a))
            .transpose()?;
        let env_json = args["env"].as_object()
            .map(|o| serde_json::to_string(o))
            .transpose()?;

        let p = UpsertParams {
            name,
            transport,
            command:       args["command"].as_str(),
            args_json,
            env_json,
            url:           args["url"].as_str(),
            api_key:       args["api_key"].as_str(),
            description:   args["description"].as_str(),
            friendly_name: args["friendly_name"].as_str(),
        };

        let tool_names = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.mcp.register(p))
        })?;

        Ok(format!(
            "MCP server '{}' registered and connected. Tools: {}",
            name,
            if tool_names.is_empty() { "(none)".to_string() } else { tool_names.join(", ") },
        ))
    }
}

// ── delete_mcp ────────────────────────────────────────────────────────────────
//
// Destructive counterpart to `register_mcp`. Kept separate from `toggle_item`
// (kind=mcp) for the same reason `delete_cron_job` is: toggling is reversible,
// deletion is not, so the distinct tool can carry its own approval rule and the
// LLM can't conflate "disable" with "remove". Both live here because both manage
// the MCP-server lifecycle and hold only `Arc<McpManager>`.

pub struct DeleteMcp {
    mcp: Arc<McpManager>,
}

impl DeleteMcp {
    pub fn new(mcp: Arc<McpManager>) -> Self { Self { mcp } }
}

impl Tool for DeleteMcp {
    fn name(&self) -> &str { "delete_mcp" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Config }

    fn description(&self) -> &str {
        "Permanently delete (unregister) an MCP server by name: removes it from the \
         database and disconnects it. This is irreversible — to temporarily turn a \
         server off without losing its configuration, use \
         `toggle_item(kind=\"mcp\", enabled=false)` instead."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the MCP server to delete (from list_items type=mcp)."
                }
            }
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let name = args["name"].as_str().unwrap_or("?");
        format!("delete MCP `{name}`")
    }

    fn execute(&self, args: Value) -> Result<String> {
        let name = args["name"].as_str()
            .ok_or_else(|| anyhow::anyhow!("delete_mcp: missing required argument `name`"))?;

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.mcp.unregister(name))
        })?;

        Ok(format!("MCP server '{name}' deleted and disconnected."))
    }
}
