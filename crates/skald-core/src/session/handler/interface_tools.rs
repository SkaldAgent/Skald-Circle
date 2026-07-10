use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use serde_json::Value;

use crate::mcp::McpManager;
use crate::tools::Tool;
use crate::tools::tool_names as tn;

pub use core_api::interface_tool::{InterfaceTool, ToolFuture};

/// All configuration for a single agent run (root or sub-agent).
///
/// Passed by reference to `run_agent_turn` and `dispatch_call_agent`.
/// Callers build this once in `handle_message`; sub-agents receive a derived
/// config with an empty `interface_tools` (except `activate_tools`) and fresh
/// `active_mcp_grants`.
pub struct AgentRunConfig {
    pub agent_id:     String,
    pub client_name:  String,
    /// Recursion depth: 0 = root agent, 1+ = sub-agent.
    pub depth:        i64,
    /// Global tool definitions (built-in tools only, no MCP, **no `Config` category**).
    /// MCP tools and the `Config` group are included dynamically in `all_tool_defs()`
    /// based on `active_mcp_grants`.
    pub base_tool_defs: Vec<Value>,
    /// Definitions of the built-in `Config`-category tools (the lazy `config` group).
    /// Appended by `all_tool_defs()` only when `active_mcp_grants` contains `"config"`.
    /// Already filtered (interactive-only / approval visibility) by the builder.
    pub config_tool_defs: Vec<Value>,
    /// Static extra context injected into the first (cacheable) system message.
    /// Example: Telegram HTML format instructions.  Should never contain
    /// per-turn data (timestamps, user-specific state) so the cached prefix
    /// remains byte-identical across turns.
    pub extra_system: Option<String>,
    /// Dynamic extra context injected as a separate system message AFTER the
    /// conversation history, just before the LLM generates its response.
    /// Example: Honcho long-term memory retrieved fresh every turn.
    /// Placing it at the tail keeps the stable prefix maximally cacheable
    /// while giving the model fresh user context at generation time.
    pub extra_system_dynamic: Option<String>,
    /// Short reminder injected as a trailing `system` message in the message list.
    pub tail_reminder: Option<String>,
    /// Named substitutions applied to the agent's system prompt at build time.
    /// Each entry replaces `__KEY__` sentinels produced by `agents::resolve_includes`.
    pub system_substitutions: HashMap<String, String>,
    /// Interface-specific tools.
    /// For sub-agents this contains only `activate_tools`; all others are dropped.
    pub interface_tools: Vec<InterfaceTool>,
    /// Tools provided by the active memory backend (e.g. `memory_query`).
    pub memory_tools: Vec<Arc<dyn Tool>>,
    /// Image generation tools — present only when at least one provider is registered.
    pub image_tools: Vec<Arc<dyn Tool>>,
    /// MCP manager — used by `all_tool_defs()` to resolve which tools to include.
    pub mcp: Arc<McpManager>,
    /// Set of MCP server names currently granted (activated) for this agent run.
    ///
    /// - Root agents: pre-populated from `session_mcp_grants` DB at config-build time;
    ///   updated in-place by `activate_tools`.
    /// - Sub-agents: starts empty; populated by `activate_tools` (stack-scoped, no
    ///   session leak); deleted from DB when the stack frame terminates.
    ///
    /// May also contain the reserved keyword `"config"`, which unlocks the built-in
    /// `Config`-category tools (`config_tool_defs`) rather than an MCP server.
    ///
    /// `all_tool_defs()` re-reads this set on every call, so tools activated via
    /// `activate_tools` in round N are available in round N+1 within the same turn.
    pub active_mcp_grants: Arc<RwLock<HashSet<String>>>,
    /// Tool names that are restricted to the root agent (depth == 0).
    /// Filtered out when deriving a sub-agent config via `for_sub_agent()`.
    pub root_only_tool_names: Vec<String>,
}

impl AgentRunConfig {
    /// Full tool list sent to the LLM on each round:
    ///   base tools  +  MCP tools for granted servers (dynamic)  +  `config` group (if granted)
    ///   +  memory tools  +  interface tools.
    ///
    /// Dynamic groups are re-queried every call so that an `activate_tools` call in
    /// round N makes the tools visible in round N+1 without rebuilding the whole config.
    pub fn all_tool_defs(&self) -> Vec<Value> {
        let mut defs = self.base_tool_defs.clone();

        // Dynamic groups: read the currently-granted set (MCP server names + `config`).
        let granted: HashSet<String> = self.active_mcp_grants
            .read()
            .map(|g| g.clone())
            .unwrap_or_default();

        // MCP servers: include tools for the granted server names.
        let servers: Vec<String> = granted.iter()
            .filter(|n| n.as_str() != crate::tools::tool_names::CONFIG_GROUP)
            .cloned()
            .collect();
        if !servers.is_empty() {
            defs.extend(
                self.mcp.tools_for(&servers)
                    .iter()
                    .map(|t| t.to_openai_definition()),
            );
        }

        // `config` group: include the built-in Config-category tools on demand.
        if granted.contains(crate::tools::tool_names::CONFIG_GROUP) {
            defs.extend(self.config_tool_defs.iter().cloned());
        }

        defs.extend(self.memory_tools.iter().map(|t| t.openai_definition()));
        defs.extend(self.image_tools.iter().map(|t| t.openai_definition()));
        defs.extend(self.interface_tools.iter().map(|t| t.definition.clone()));
        defs
    }

    /// Derives a config for a sub-agent:
    /// - Inherits base tools, memory tools, and MCP manager.
    /// - Starts with **empty** `active_mcp_grants` (sub-agents activate what they need).
    /// - Drops all interface tools (caller re-injects `activate_tools` explicitly).
    /// - Increments depth.
    pub fn for_sub_agent(&self, agent_id: String, client_name: String) -> Self {
        let root_only = |defs: &mut Vec<Value>| {
            defs.retain(|def| {
                let name = def["function"]["name"].as_str().unwrap_or("");
                !self.root_only_tool_names.iter().any(|n| n == name)
            });
        };

        let mut defs = self.base_tool_defs.clone();
        root_only(&mut defs);
        // Strip the per-level augmentations that the config builders re-derive, so
        // they are never inherited: `ask_user_clarification` is added by
        // `build_agent_config` (root) and re-added by `dispatch_sub_agent`;
        // `execute_subtask` is added by `dispatch_sub_agent`. Leaving them in the
        // inherited set would duplicate them (depth ≥ 1 for `ask_user_clarification`,
        // depth ≥ 2 for `execute_subtask`) and the OpenAI-compat APIs reject
        // non-unique tool names with HTTP 400. With this strip, `dispatch_sub_agent`
        // is the single owner of sub-agent augmentation and duplication is
        // structurally impossible — no dedup pass needed anywhere.
        {
            const RE_DERIVED: &[&str] = &[tn::ASK_USER_CLARIFICATION, tn::EXECUTE_SUBTASK];
            defs.retain(|d| {
                let name = d["function"]["name"].as_str().unwrap_or("");
                !RE_DERIVED.contains(&name)
            });
        }

        // Inherit the (already filtered) `config` group, dropping any root-only tool.
        let mut config_defs = self.config_tool_defs.clone();
        root_only(&mut config_defs);

        Self {
            agent_id,
            client_name,
            depth:                self.depth + 1,
            base_tool_defs:       defs,
            config_tool_defs:     config_defs,
            extra_system:         None,
            extra_system_dynamic: None,
            tail_reminder:        None,
            system_substitutions: HashMap::new(),
            interface_tools:      vec![],
            memory_tools:         self.memory_tools.clone(),
            image_tools:          self.image_tools.clone(),
            mcp:                  Arc::clone(&self.mcp),
            active_mcp_grants:    Arc::new(RwLock::new(HashSet::new())),
            root_only_tool_names: self.root_only_tool_names.clone(),
        }
    }
}
