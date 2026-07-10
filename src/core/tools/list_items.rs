use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::core::agents;
use crate::core::cron::TaskManager;
use crate::core::mcp::McpManager;
use crate::core::plugin::PluginManager;
use crate::core::tools::{Tool, ToolDescriptionLength};

/// Unified read-only listing tool. Replaces the per-resource `list_mcp`,
/// `list_plugins`, `list_cron_jobs` and `list_agents` tools: same operation
/// (enumerate), uniform schema (a single `type` discriminator), so it merges
/// cleanly without losing schema-level validation.
///
/// `list_secrets` is intentionally NOT folded in — it preserves a name-based
/// access-control boundary (an agent granted `list_items` must not thereby gain
/// the ability to enumerate secret key names) and carries a `pattern` filter
/// that would only apply to that one type.
pub struct ListItems {
    mcp:     Arc<McpManager>,
    plugins: Arc<PluginManager>,
    cron:    Arc<TaskManager>,
}

impl ListItems {
    pub fn new(mcp: Arc<McpManager>, plugins: Arc<PluginManager>, cron: Arc<TaskManager>) -> Self {
        Self { mcp, plugins, cron }
    }
}

impl Tool for ListItems {
    fn name(&self) -> &str { "list_items" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Introspection }

    fn description(&self) -> &str {
        "List configured items of a given type. Pass `type`:\n\
         • `mcp` — MCP servers with status (running, error, disabled), description, friendly_name, and exposed tools.\n\
         • `plugins` — plugins with id, name, description, enabled flag (persisted), and running flag (live).\n\
         • `cron` — scheduled tasks/cron jobs with id, title, cron expression, agent_id, enabled, kind, last/next run.\n\
         • `agents` — sub-agents available to delegate to (id, name, description, optional `instructions` on how to call the agent well, optional client). Do NOT invoke the `main` agent.\n\
         To list stored secret names use `list_secrets` instead."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["type"],
            "properties": {
                "type": {
                    "type": "string",
                    "enum": ["mcp", "plugins", "cron", "agents"],
                    "description": "Which kind of item to list."
                }
            }
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let kind = args["type"].as_str().unwrap_or("?");
        format!("list {kind}")
    }

    fn execute(&self, args: Value) -> Result<String> {
        let kind = args["type"].as_str()
            .ok_or_else(|| anyhow::anyhow!("list_items: missing required argument `type`"))?;

        match kind {
            "mcp" => {
                let infos = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(self.mcp.list())
                })?;
                Ok(serde_json::to_string_pretty(&infos)?)
            }
            "plugins" => {
                let plugins = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(self.plugins.list())
                })?;
                Ok(serde_json::to_string_pretty(&plugins)?)
            }
            "cron" => {
                let jobs = self.cron.list_jobs()?;
                if jobs.is_empty() {
                    return Ok("No tasks configured.".into());
                }
                let arr: Vec<Value> = jobs.iter().map(|j| json!({
                    "id":          j.id,
                    "title":       j.title,
                    "description": j.description,
                    "cron":        j.cron,
                    "agent_id":    j.agent_id,
                    "enabled":     j.enabled,
                    "single_run":  j.single_run,
                    "kind":        j.kind,
                    "last_run_at": j.last_run_at,
                    "next_run_at": j.next_run_at,
                    "created_at":  j.created_at,
                })).collect();
                Ok(serde_json::to_string_pretty(&arr)?)
            }
            "agents" => {
                let mut list = agents::discover()?;
                // Only dispatchable task agents are listed; chat + system are excluded.
                list.retain(|a| a.agent_type == agents::AgentType::Task);
                let arr: Vec<Value> = list
                    .into_iter()
                    .map(|a| {
                        let mut o = serde_json::Map::new();
                        o.insert("id".into(),          Value::String(a.id));
                        o.insert("name".into(),        Value::String(a.name));
                        o.insert("description".into(), Value::String(a.description));
                        // `instructions` (how to call the agent well) is surfaced here only,
                        // and only when set — eager but scoped to task agents (already the
                        // sole agents listed above).
                        if let Some(i) = a.instructions {
                            o.insert("instructions".into(), Value::String(i));
                        }
                        if let Some(c) = a.client {
                            o.insert("client".into(), Value::String(c));
                        }
                        Value::Object(o)
                    })
                    .collect();
                Ok(serde_json::to_string_pretty(&arr)?)
            }
            other => anyhow::bail!("list_items: unknown type `{other}` (expected one of: mcp, plugins, cron, agents)"),
        }
    }
}
