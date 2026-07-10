use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::mcp::McpManager;
use crate::tools::{ToolCategory, ToolDescriptionLength, ToolRegistry};
use crate::tools::tool_names as tn;

#[derive(Debug, Clone, Serialize)]
pub struct ToolInfo {
    pub name:        String,
    pub description: String,
    pub source:      String,
    pub server:      Option<String>,
    pub category:    Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct McpServerMeta {
    pub friendly_name: Option<String>,
    pub description:   Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AllTools {
    pub built_in:    Vec<ToolInfo>,
    pub mcp:         Vec<ToolInfo>,
    /// server internal name → metadata (friendly_name, description).
    /// Populated by the API handler via a DB query; empty when constructed here.
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerMeta>,
}

pub struct ToolCatalog {
    tools: Arc<ToolRegistry>,
    mcp:   Arc<McpManager>,
}

impl ToolCatalog {
    pub fn new(tools: Arc<ToolRegistry>, mcp: Arc<McpManager>) -> Self {
        Self { tools, mcp }
    }

    pub fn list_all(&self) -> AllTools {
        let mut built_in: Vec<ToolInfo> = self.tools
            .list_all()
            .into_iter()
            .map(|(name, description)| {
                let category = self.tools.category_of(&name).map(category_str);
                ToolInfo { name, description, source: "built-in".into(), server: None, category }
            })
            .collect();

        for (name, description, category) in Self::synthetic_tools() {
            built_in.push(ToolInfo {
                name:        (*name).to_string(),
                description: (*description).to_string(),
                source:      "built-in".into(),
                server:      None,
                category:    Some((*category).to_string()),
            });
        }

        built_in.sort_by(|a, b| a.name.cmp(&b.name));

        let mcp: Vec<ToolInfo> = self.mcp
            .tools()
            .into_iter()
            .map(|t| ToolInfo {
                name:        t.tool_id(),
                description: t.description,
                source:      "mcp".into(),
                server:      Some(t.server_name),
                category:    None,
            })
            .collect();

        AllTools { built_in, mcp, mcp_servers: HashMap::new() }
    }

    pub fn describe_call(&self, name: &str, args: &Value, length: ToolDescriptionLength) -> String {
        self.tools.describe_call(name, args, length)
    }

    /// Core-owned tools that are injected per-session outside the `ToolRegistry`
    /// (interface tools + the provider-gated `image_generate`), listed statically
    /// so they can be pre-configured in the Security-groups UI *before* first use.
    ///
    /// This is a best-effort eager list — correctness does not depend on it being
    /// complete: `ToolDiscovery` surfaces any tool that is actually offered, and
    /// the catch-all `* require` gates anything not yet configured. Only names the
    /// core legitimately owns belong here; plugin/provider tool names are left to
    /// discovery so core stays decoupled from them.
    fn synthetic_tools() -> &'static [(&'static str, &'static str, &'static str)] {
        &[
            (tn::EXECUTE_TASK,    "Delegate to / schedule a sub-agent (cron, sync=inline sub-agent, async=background).", "subagent"),
            (tn::EXECUTE_SUBTASK, "Run a synchronous sub-task inside a background session.", "subagent"),
            (tn::UPDATE_SCRATCHPAD,      "Write a key-value note into the session scratchpad.", "introspection"),
            (tn::ASK_USER_CLARIFICATION, "Pause and ask the user a clarification question.", "introspection"),
            (tn::WRITE_TODOS,       "Record and update the agent's private per-turn task list.", "introspection"),
            (tn::ACTIVATE_TOOLS,    "Unlock an MCP server's tools or the built-in config tool group for this session.", "config"),
            (tn::NOTIFY,            "Send a proactive notification to the user (background/event-triage sessions).", "introspection"),
            (tn::SHOW_FILE_TO_USER, "Open a file in the user's viewer (web/mobile sessions).", "introspection"),
            (tn::IMAGE_GENERATE,    "Generate an image from a text prompt (requires an image provider).", "config"),
        ]
    }
}

fn category_str(cat: ToolCategory) -> String {
    match cat {
        ToolCategory::Filesystem    => "filesystem",
        ToolCategory::Shell         => "shell",
        ToolCategory::Subagent      => "subagent",
        ToolCategory::Introspection => "introspection",
        ToolCategory::Config        => "config",
    }.to_string()
}
