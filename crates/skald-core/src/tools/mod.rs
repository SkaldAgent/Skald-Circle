/// Tools that write or modify files on disk.
/// Used by the approval gate (diff preview logic) and the LLM loop (FileChanged events).
/// Update this list whenever a new file-write tool is added.
pub const FILE_WRITE_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "insert_at_line",
    "replace_lines",
];

/// Returns `true` if `name` is a file-write tool (i.e. it modifies files on disk).
pub fn is_file_write_tool(name: &str) -> bool {
    FILE_WRITE_TOOLS.contains(&name)
}

/// Tools that read file contents or directory listings from disk.
/// Used by the approval gate to apply the `RunContext` read fast-path (auto-allow
/// working dir / `docs/` / `skills/` / `allow_fs_reads`). All take a `path` argument.
/// Update this list whenever a new file-read tool is added.
pub const FILE_READ_TOOLS: &[&str] = &[
    "read_file",
    "grep_files",
    "list_files",
    "search_file",
    "get_ast_outline",
];

/// Returns `true` if `name` is a file-read tool (i.e. it reads files/dirs from disk).
pub fn is_file_read_tool(name: &str) -> bool {
    FILE_READ_TOOLS.contains(&name)
}

pub mod tool_names;
pub mod activate_tools;
pub mod ast_outline;
pub mod configure_plugin;
pub mod cron_jobs;
pub mod exec;
pub mod fs;
pub mod image_generate;
pub mod list_items;
pub mod list_secrets;
pub mod notify;
pub mod set_secret;
pub mod read_notification;
pub mod show_file;
pub mod toggle_item;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

pub use core_api::tool::{
    drive_execution, ExecutionOutcome, MediaRef, SimpleExecution, Tool, ToolCategory, ToolContext,
    ToolDescriptionLength, ToolExecution, ToolResult, truncate_label,
};


pub const MAX_LABEL_SHORT: usize = 60;
pub const MAX_LABEL_FULL: usize = 120;

/// UI metadata for one tool call — the friendly card title plus a **semantic** icon
/// key (never a glyph; the frontend maps the key to an icon + accent color). Computed
/// by [`ToolRegistry::display_meta`], the single seam shared by the live WS event and
/// the history projection so the two can't drift.
#[derive(Debug, Clone)]
pub struct ToolUiMeta {
    pub display_name: String,
    pub icon: String,
}

/// Turns a raw tool id (`snake_case` / `kebab-case`) into a Title-Cased phrase for a
/// UI label when no friendly name is declared — `list_recent_files` → "List Recent
/// Files". The last-resort fallback for MCP and unknown tools.
pub fn prettify_tool_name(name: &str) -> String {
    name.split(|c| c == '_' || c == '-')
        .filter(|s| !s.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Registry of all available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: HashMap::new() }
    }

    pub fn register(&mut self, tool: impl Tool + 'static) {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
    }

    /// Register an already-boxed tool (e.g. plugin-provided tools whose
    /// constructors return `Arc<dyn Tool>`).
    pub fn register_arc(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Tool definitions for the root agent (depth = 0): excludes sub_agents_only tools.
    pub fn openai_definitions(&self) -> Vec<Value> {
        self.tools.values()
            .filter(|t| !t.sub_agents_only())
            .map(|t| t.openai_definition())
            .collect()
    }

    /// Like [`openai_definitions`], but **excludes** `Config`-category tools.
    /// These are lazy-loaded on demand via `activate_tools(["config"])`, so they
    /// are not part of the always-on base tool set.
    pub fn openai_definitions_excluding_config(&self) -> Vec<Value> {
        self.tools.values()
            .filter(|t| !t.sub_agents_only() && t.category() != ToolCategory::Config)
            .map(|t| t.openai_definition())
            .collect()
    }

    /// Definitions of the `Config`-category tools only (the lazy `config` group).
    /// Injected dynamically by `all_tool_defs()` when the `config` group is granted.
    pub fn openai_definitions_config_only(&self) -> Vec<Value> {
        self.tools.values()
            .filter(|t| !t.sub_agents_only() && t.category() == ToolCategory::Config)
            .map(|t| t.openai_definition())
            .collect()
    }

    /// Tool definitions that are marked sub_agents_only. Used in dispatch_call_agent
    /// to augment the child config's base_tool_defs.
    pub fn openai_definitions_sub_agents_only(&self) -> Vec<Value> {
        self.tools.values()
            .filter(|t| t.sub_agents_only())
            .map(|t| t.openai_definition())
            .collect()
    }

    /// Returns the names of all tools marked `root_agent_only`.
    pub fn root_agent_only_names(&self) -> Vec<String> {
        self.tools.values()
            .filter(|t| t.root_agent_only())
            .map(|t| t.name().to_string())
            .collect()
    }

    /// Returns the names of all tools marked `interactive_only`.
    pub fn interactive_only_names(&self) -> Vec<String> {
        self.tools.values()
            .filter(|t| t.interactive_only())
            .map(|t| t.name().to_string())
            .collect()
    }

    /// Returns `(name, description)` for every registered tool.
    pub fn list_all(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self.tools.values()
            .map(|t| (t.name().to_string(), t.description().to_string()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Human-readable label for any tool call, including non-registry tools (call_agent, MCP, …).
    pub fn describe_call(&self, name: &str, args: &Value, length: ToolDescriptionLength) -> String {
        if let Some(tool) = self.tools.get(name) {
            return tool.describe(args, length);
        }
        // Non-registry tools handled inline. `show_file_to_user` is an InterfaceTool
        // (injected in ws.rs), so it has no registry `describe`; surface its target
        // path in the label so the frontend renders it as a clickable file link.
        if name == tool_names::SHOW_FILE_TO_USER {
            if let Some(path) = args["path"].as_str() {
                let max = match length {
                    ToolDescriptionLength::Short => MAX_LABEL_SHORT,
                    ToolDescriptionLength::Full  => MAX_LABEL_FULL,
                };
                return truncate_label(&format!("{name} `{path}`"), max);
            }
        }
        // Sub-agent delegation tools (`execute_task`, `execute_subtask`, and the
        // legacy `run_subtask` alias) are InterfaceTools, not in the registry.
        // Surface agent_id + description so the UI/Telegram shows what is being
        // delegated instead of the bare tool name.
        if name == tool_names::EXECUTE_TASK
            || name == tool_names::EXECUTE_SUBTASK
            || name == "run_subtask"
        {
            return describe_sub_agent_call(name, args, length);
        }
        name.to_string()
    }

    /// Friendly card metadata (display name + semantic icon key) for any tool call,
    /// including non-registry tools. Registry tools delegate to
    /// [`Tool::display_name`]/[`Tool::icon`]; sub-agent and interface tools are
    /// handled inline; an `mcp__server__tool` name gets `icon = "mcp"` and a
    /// prettified tool name — the caller (which holds the [`McpProvider`]) may then
    /// override `display_name` with the connector's resolved friendly name.
    ///
    /// [`McpProvider`]: crate::mcp::provider::McpProvider
    pub fn display_meta(&self, name: &str, args: &Value) -> ToolUiMeta {
        if let Some(tool) = self.tools.get(name) {
            return ToolUiMeta {
                display_name: tool.display_name().to_string(),
                icon: tool.icon().to_string(),
            };
        }
        // Sub-agent delegation tools (InterfaceTools, not in the registry).
        if name == tool_names::EXECUTE_TASK
            || name == tool_names::EXECUTE_SUBTASK
            || name == "run_subtask"
        {
            let dn = match args["agent_id"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                Some(a) => format!("Sub-agent: {a}"),
                None => "Sub-agent".to_string(),
            };
            return ToolUiMeta { display_name: dn, icon: "subagent".to_string() };
        }
        if name == tool_names::SHOW_FILE_TO_USER {
            return ToolUiMeta { display_name: "Show File".to_string(), icon: "read".to_string() };
        }
        // MCP tool `mcp__server__tool`: default to a prettified tool name; the caller
        // overrides with the connector's manifest/`title` friendly name when known.
        if let Some(rest) = name.strip_prefix("mcp__") {
            let tool = rest.split_once("__").map(|(_, t)| t).unwrap_or(rest);
            return ToolUiMeta { display_name: prettify_tool_name(tool), icon: "mcp".to_string() };
        }
        ToolUiMeta { display_name: prettify_tool_name(name), icon: "tool".to_string() }
    }

    /// Returns the category of a registered tool, or `None` for unknown tools
    /// (MCP tools, interface tools, call_agent, etc.).
    pub fn category_of(&self, name: &str) -> Option<ToolCategory> {
        self.tools.get(name).map(|t| t.category())
    }

    /// Path to a single viewable file targeted by this tool call, if any.
    /// `None` for non-file tools, directory tools, and unknown/non-registry tools.
    ///
    /// `show_file_to_user` is an InterfaceTool (not in the registry) whose whole
    /// purpose is to open a file, so it is handled inline here as well — mirroring
    /// `describe_call`, so its label and clickable path use the same raw `path` arg.
    pub fn target_path(&self, name: &str, args: &Value) -> Option<String> {
        if let Some(tool) = self.tools.get(name) {
            return tool.target_path(args);
        }
        if name == tool_names::SHOW_FILE_TO_USER {
            return args["path"].as_str().map(str::to_string);
        }
        None
    }

    /// Dispatch a tool call by name.
    pub async fn dispatch(&self, name: &str, args: Value) -> Result<String> {
        match self.tools.get(name) {
            Some(tool) => tool.execute_async(args).await,
            None       => anyhow::bail!("Unknown tool: {name}"),
        }
    }

    /// Start a [`ToolExecution`] for a registered tool, or `None` if `name` is not
    /// in the registry (MCP / interface tools are handled by the caller). The
    /// returned handle borrows the registry, which outlives the turn.
    ///
    /// `ctx` carries the caller's session id and owner pool: owner-bound tools
    /// (e.g. cron management) act on `ctx.pool` instead of a globally-captured
    /// manager. Context-free tools ignore it via the default `run_with`.
    pub fn run(&self, name: &str, ctx: &ToolContext, args: Value) -> Option<Box<dyn ToolExecution + '_>> {
        self.tools.get(name).map(|tool| tool.run_with(ctx, args))
    }
}

/// Builds a human-readable label for a sub-agent delegation call
/// (`execute_task` / `execute_subtask` / legacy `run_subtask`), all of which are
/// InterfaceTools outside the registry. Shows `agent_id` + `description` (falling
/// back to `title`, then to the bare name) so the UI/Telegram displays what is
/// being delegated. When `mode` is present (only `execute_task` carries it) a
/// single emoji is appended to the tool name as a compact mode marker:
/// sync → ⚡, async → 🚀, cron → 📅.
fn describe_sub_agent_call(name: &str, args: &Value, length: ToolDescriptionLength) -> String {
    let agent_id = args["agent_id"].as_str().map(|s| s.trim()).unwrap_or("");
    let subject = args["description"].as_str()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .or_else(|| args["title"].as_str().map(|s| s.trim()).filter(|s| !s.is_empty()))
        .unwrap_or("");
    let mode_emoji = match args["mode"].as_str().map(|s| s.trim()) {
        Some("sync")  => Some("⚡"),
        Some("async") => Some("🚀"),
        Some("cron")  => Some("📅"),
        _             => None,
    };

    let max = match length {
        ToolDescriptionLength::Short => MAX_LABEL_SHORT,
        ToolDescriptionLength::Full  => MAX_LABEL_FULL,
    };

    let prefix = match mode_emoji {
        Some(e) => format!("{name} {e}"),
        None    => name.to_string(),
    };

    let label = match (agent_id.is_empty(), subject.is_empty()) {
        (false, false) => format!("{prefix} → {agent_id}: {subject}"),
        (false, true)  => format!("{prefix} → {agent_id}"),
        _              => prefix,
    };

    truncate_label(&label, max)
}
