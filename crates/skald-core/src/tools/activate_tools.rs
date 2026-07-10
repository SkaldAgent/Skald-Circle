use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use anyhow::Result;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::mcp::McpManager;
use crate::tools::tool_names::CONFIG_GROUP;
use crate::tools::{Tool, ToolDescriptionLength, truncate_label, MAX_LABEL_SHORT};

/// Per-session (or per-stack) tool that activates **tool groups** on demand.
///
/// A group is either:
/// - an **MCP server name** — loads that server's tools, or
/// - the reserved keyword `"config"` — loads all built-in `Config`-category
///   tools (system configuration: MCP/plugin/cron management, secrets).
///
/// When the LLM calls `activate_tools(["gmail", "config"])`:
/// - The in-memory grant set is updated immediately, so the group's tools appear
///   in the *next LLM round* of the current turn (via `all_tool_defs()`).
/// - If `stack_id` is `None` (root agent): grants are persisted to
///   `session_mcp_grants` — they survive across turns and restarts.
/// - If `stack_id` is `Some(id)` (sub-agent): grants are persisted to
///   `stack_mcp_grants` for that stack frame — they survive restarts but are
///   deleted when the frame terminates (`dispatch_call_agent` calls
///   `stack_mcp_grants::delete_for_stack` on cleanup).
///
/// The `session_mcp_grants` / `stack_mcp_grants` tables store the group string
/// verbatim, so `"config"` is persisted just like an MCP server name.
///
/// Not in the global `ToolRegistry` — injected as an `InterfaceTool` in
/// `build_agent_config` (root) and `dispatch_call_agent` (sub-agents).
pub struct ActivateTools {
    pub pool:               Arc<SqlitePool>,
    pub session_id:         i64,
    /// `None` for root agents (session-scoped grants).
    /// `Some(stack_id)` for sub-agents (stack-scoped grants, deleted on frame exit).
    pub stack_id:           Option<i64>,
    pub mcp:                Arc<McpManager>,
    /// Shared in-memory grant set. Updated in-place on every call so subsequent
    /// rounds within the same turn see the new tools via `all_tool_defs()`.
    pub active_mcp_grants:  Arc<RwLock<HashSet<String>>>,
}

impl Tool for ActivateTools {
    fn name(&self) -> &str { crate::tools::tool_names::ACTIVATE_TOOLS }

    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Config }

    fn description(&self) -> &str {
        "Activate one or more tool groups so their tools become available. \
         A group is either an MCP server name (see the MCP list) or the reserved \
         keyword `config`, which loads all system-configuration tools (managing \
         MCP servers, plugins, scheduled cron jobs, and secrets). \
         Pass an array of group names. \
         Once activated, the tools are available from the next tool-call round onward."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "groups": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Tool groups to activate: MCP server names and/or the reserved \
                                    keyword \"config\" (e.g. [\"gmail\", \"config\"])."
                }
            },
            "required": ["groups"]
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let names = args["groups"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
            .unwrap_or_else(|| "?".to_string());
        truncate_label(&format!("activate tools [{names}]"), MAX_LABEL_SHORT)
    }

    fn execute(&self, args: Value) -> Result<String> {
        let names: Vec<String> = args["groups"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("activate_tools: `groups` must be an array"))?
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();

        if names.is_empty() {
            anyhow::bail!("activate_tools: `groups` is empty");
        }

        let available: HashSet<String> = self.mcp.tools()
            .iter()
            .map(|t| t.server_name.clone())
            .collect();

        let pool       = Arc::clone(&self.pool);
        let session_id = self.session_id;
        let stack_id   = self.stack_id;
        let grants_set = Arc::clone(&self.active_mcp_grants);

        // Persist to DB (session-scoped or stack-scoped) and update in-memory set.
        // The reserved `config` group is stored verbatim, exactly like a server name.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                for name in &names {
                    match stack_id {
                        None => {
                            crate::db::session_mcp_grants::grant(&pool, session_id, name).await?;
                        }
                        Some(sid) => {
                            crate::db::stack_mcp_grants::grant(&pool, sid, name).await?;
                        }
                    }
                }
                anyhow::Ok(())
            })
        })?;

        // Update in-memory set so the next LLM round sees the new grants.
        {
            let mut set = grants_set.write()
                .map_err(|_| anyhow::anyhow!("activate_tools: lock poisoned"))?;
            for name in &names {
                set.insert(name.clone());
            }
        }

        let activated: Vec<String> = names.iter()
            .map(|n| {
                if n == CONFIG_GROUP {
                    // Built-in group — always available, no MCP server to reconnect.
                    format!("{n} ✓")
                } else if available.contains(n) {
                    format!("{n} ✓")
                } else {
                    format!("{n} (registered but not yet running — tools will appear after reconnect)")
                }
            })
            .collect();

        let scope = match stack_id {
            None    => "session".to_string(),
            Some(s) => format!("stack {s}"),
        };

        Ok(format!(
            "Tool groups activated for this {scope}: {}. \
             Their tools are available from the next tool-call round.",
            activated.join(", ")
        ))
    }
}
