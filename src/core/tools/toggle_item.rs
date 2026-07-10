use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::core::cron::TaskManager;
use crate::core::mcp::McpManager;
use crate::core::plugin::PluginManager;
use crate::core::tools::{Tool, ToolDescriptionLength};

/// Unified enable/disable tool. Replaces `toggle_mcp`, `toggle_plugin` and
/// `toggle_cron_job`: same operation (flip an enabled flag), uniform schema
/// (`kind` + `id` + `enabled`), all `required` validatable at schema level.
///
/// `delete_cron_job` is intentionally NOT folded in — it is destructive
/// (irreversible) whereas toggling is reversible, and keeping it separate lets
/// it carry a distinct approval rule.
pub struct ToggleItem {
    mcp:     Arc<McpManager>,
    plugins: Arc<PluginManager>,
    cron:    Arc<TaskManager>,
}

impl ToggleItem {
    pub fn new(mcp: Arc<McpManager>, plugins: Arc<PluginManager>, cron: Arc<TaskManager>) -> Self {
        Self { mcp, plugins, cron }
    }
}

impl Tool for ToggleItem {
    fn name(&self) -> &str { "toggle_item" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Config }

    fn description(&self) -> &str {
        "Enable or disable an item by kind. Pass `kind`, `id`, and `enabled`:\n\
         • `mcp` — `id` is the server name. NOTE: a restart is required for the change to take full effect on running servers.\n\
         • `plugin` — `id` is the plugin id (e.g. \"telegram\"). Takes effect immediately (the plugin is started/stopped at once).\n\
         • `cron` — `id` is the numeric job id (from `list_items` type=cron). Re-enabling recalculates next_run_at.\n\
         Use `list_items` to find current names/ids and statuses."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["kind", "id", "enabled"],
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["mcp", "plugin", "cron"],
                    "description": "Which kind of item to toggle."
                },
                "id": {
                    "type": "string",
                    "description": "MCP server name | plugin id | numeric cron job id (as a string)."
                },
                "enabled": {
                    "type": "boolean",
                    "description": "true to enable, false to disable."
                }
            }
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let kind    = args["kind"].as_str().unwrap_or("?");
        let id      = args["id"].as_str().unwrap_or("?");
        let enabled = args["enabled"].as_bool().unwrap_or(true);
        let action  = if enabled { "enable" } else { "disable" };
        format!("{action} {kind} `{id}`")
    }

    fn execute(&self, args: Value) -> Result<String> {
        let kind = args["kind"].as_str()
            .ok_or_else(|| anyhow::anyhow!("toggle_item: missing required argument `kind`"))?;
        let id = args["id"].as_str()
            .ok_or_else(|| anyhow::anyhow!("toggle_item: missing required argument `id`"))?;
        let enabled = args["enabled"].as_bool()
            .ok_or_else(|| anyhow::anyhow!("toggle_item: missing required argument `enabled`"))?;

        match kind {
            "mcp" => {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(self.mcp.set_enabled(id, enabled))
                })?;
                Ok(format!(
                    "MCP server '{}' is now {}. Note: a restart is required for the change to take effect on running servers.",
                    id,
                    if enabled { "enabled" } else { "disabled" }
                ))
            }
            "plugin" => {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(self.plugins.toggle(id, enabled))
                })?;
                Ok(format!(
                    "Plugin '{}' is now {}.",
                    id,
                    if enabled { "enabled and running" } else { "disabled and stopped" }
                ))
            }
            "cron" => {
                let job_id = id.parse::<i64>()
                    .map_err(|_| anyhow::anyhow!("toggle_item: for kind=cron, `id` must be a numeric job id (got '{id}')"))?;
                if self.cron.toggle_job(job_id, enabled)? {
                    Ok(format!("Task {job_id} {}.", if enabled { "enabled" } else { "disabled" }))
                } else {
                    Ok(format!("No task with id {job_id}."))
                }
            }
            other => anyhow::bail!("toggle_item: unknown kind `{other}` (expected one of: mcp, plugin, cron)"),
        }
    }
}
