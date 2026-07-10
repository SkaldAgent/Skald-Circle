use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::core::plugin::PluginManager;
use crate::core::tools::{Tool, ToolDescriptionLength};

pub struct ConfigurePlugin(pub Arc<PluginManager>);

impl Tool for ConfigurePlugin {
    fn name(&self) -> &str { "configure_plugin" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Config }

    fn description(&self) -> &str {
        "Update the configuration of a plugin and restart it immediately. \
         Use `list_items` (type=plugins) to see available plugin ids and their config schemas. \
         The config object must match the plugin's schema — extra keys are ignored."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type":        "string",
                    "description": "Plugin id (e.g. \"remote_connectivity\", \"telegram\")."
                },
                "config": {
                    "type":        "object",
                    "description": "Config object matching the plugin's schema. Existing keys not provided here are cleared.",
                    "additionalProperties": true
                },
                "enabled": {
                    "type":        "boolean",
                    "description": "Whether to enable the plugin. Defaults to true.",
                    "default":     true
                }
            },
            "required": ["id", "config"]
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let id      = args["id"].as_str().unwrap_or("?");
        let enabled = args["enabled"].as_bool().unwrap_or(true);
        let action  = if enabled { "configure" } else { "disable" };
        format!("{action} plugin `{id}`")
    }

    fn execute(&self, args: Value) -> Result<String> {
        let id = args["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("configure_plugin: missing required argument `id`"))?;
        let config = args["config"].clone();
        if !config.is_object() {
            anyhow::bail!("configure_plugin: `config` must be an object");
        }
        let enabled = args["enabled"].as_bool().unwrap_or(true);

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(self.0.update_config(id, enabled, config))
        })?;

        Ok(format!(
            "Plugin '{}' configured and {}.",
            id,
            if enabled { "started" } else { "stopped" }
        ))
    }
}
