use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use serde_json::Value;

use crate::tools::tool_names as tn;
use super::{ChatSessionHandler, update_scratchpad_tool_def, write_todos_tool_def};
use super::interface_tools::{AgentRunConfig, InterfaceTool, ToolFuture};

/// Returns an `activate_tools` OpenAI tool definition.
pub(super) fn activate_tools_tool_def() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": tn::ACTIVATE_TOOLS,
            "description": "Activate one or more tool groups so their tools become available. \
                            A group is either an MCP server name (see the MCP list) or the reserved \
                            keyword `config`, which loads all system-configuration tools (managing \
                            MCP servers, plugins, scheduled cron jobs, and secrets). \
                            Pass an array of group names (e.g. [\"gmail\", \"config\"]). \
                            Once activated, the tools are available from the next tool-call round onward.",
            "parameters": {
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
            }
        }
    })
}

impl ChatSessionHandler {
    /// Resolves the LLM client and assembles `AgentRunConfig` for a top-level turn
    /// (depth = 0). Extracted to avoid duplicating the same ~15 lines in both
    /// `handle_message` and `resume_turn`.
    pub(super) async fn build_agent_config(
        &self,
        client_name:          Option<String>,
        extra_system:         Option<String>,
        extra_system_dynamic: Option<String>,
        mut interface_tools:  Vec<InterfaceTool>,
        system_substitutions: HashMap<String, String>,
    ) -> anyhow::Result<AgentRunConfig> {
        let meta = crate::agents::load_meta(&self.agent_id).ok();
        let (key, _) = self.llm_manager.resolve(
            client_name.as_deref(),
            meta.as_ref().and_then(|m| m.strength),
        ).await?;

        let mut base_tool_defs = self.tools.openai_definitions_excluding_config();
        // Config-category built-ins are hidden from the always-on set and lazy-loaded
        // via `activate_tools(["config"])`. They go through the same interactive-only /
        // approval-visibility filters as base_tool_defs below, then ride in AgentRunConfig
        // as `config_tool_defs` (appended by `all_tool_defs()` only when granted).
        let mut config_tool_defs = self.tools.openai_definitions_config_only();
        base_tool_defs.push(update_scratchpad_tool_def());
        base_tool_defs.push(write_todos_tool_def());
        // `ask_user_clarification` is available to every agent except hidden `system`
        // agents (e.g. TIC), which have no user-facing channel. Interactive sessions
        // emit AgentQuestion inline (plus the Inbox); background sessions rely on the
        // Inbox alone.
        let is_system = meta
            .as_ref()
            .map(|m| m.agent_type == crate::agents::AgentType::System)
            .unwrap_or(false);
        if !is_system {
            base_tool_defs.push(super::ask_user_clarification_tool_def());
        }

        // Background sessions (cron, tic): remove tools that only make sense in
        // interactive sessions (e.g. read_notification, which is synthetically
        // injected by ChatHub and returns EMPTY if called directly).
        if !self.is_interactive {
            let interactive_only = self.tools.interactive_only_names();
            let keep = |def: &Value| {
                let name = def["function"]["name"].as_str().unwrap_or("");
                !interactive_only.iter().any(|n| n == name)
            };
            base_tool_defs.retain(|d| keep(d));
            config_tool_defs.retain(|d| keep(d));
        }
        // Interactive sessions get read_agent_result so the LLM can poll for async
        // task status. The real delivery happens via inject_async_result (synthetic msg).
        if self.is_interactive {
            base_tool_defs.push(serde_json::json!({
                "type": "function",
                "function": {
                    "name": "task_completed",
                    "description": "Invoked BY THE SYSTEM (not by you) when an async task finishes, \
                                    delivering its result. You will never need to call this yourself — \
                                    the system calls it automatically when execute_task(mode=async) completes.",
                    "parameters": {
                        "type": "object",
                        "required": ["task_id"],
                        "properties": {
                            "task_id": { "type": "integer", "description": "The completed task id" }
                        }
                    }
                }
            }));
        }

        // Approval-rules visibility filter: hide tools whose effective action for
        // this session's permission group is Deny. Rules are loaded once and applied
        // synchronously; the execution-time gate in ApprovalManager remains as a
        // second layer of enforcement.
        {
            let group_id   = self.tool_group_id().await;
            let gid        = group_id.as_deref().unwrap_or("default");
            // `approval_rules` is a registry table (`create_registry_tables`), so it
            // must be read from the registry pool, not the per-user owner pool — the
            // latter has no such table, the query errors, and `unwrap_or_default()`
            // would silently yield an empty ruleset (→ every tool "visible").
            let group_rules = match crate::db::approval_rules::list_for_group(
                &self.shared_pool, Some(gid),
            ).await {
                Ok(rules) => rules,
                Err(e) => {
                    tracing::warn!(group = gid, error = %e, "approval-rules visibility filter: list_for_group failed; leaving all tools visible");
                    Vec::new()
                }
            };
            let visible = |def: &Value| {
                let name = def["function"]["name"].as_str().unwrap_or("");
                self.approval.is_tool_visible(&group_rules, name)
            };
            base_tool_defs.retain(|d| visible(d));
            config_tool_defs.retain(|d| visible(d));
        }

        // ── Tool-group grant initialisation ─────────────────────────────────────
        //
        // Load persisted session grants from DB (MCP server names and/or the reserved
        // `config` keyword), then inject `activate_tools` so the LLM can activate
        // additional groups on demand.
        let persisted = crate::db::activated_tools::list_refs_session(
            &self.db, self.session_id,
        ).await.unwrap_or_default();

        let active_mcp_grants: Arc<RwLock<HashSet<String>>> =
            Arc::new(RwLock::new(persisted.into_iter().collect()));

        {
            let mcp_clone    = Arc::clone(&self.mcp);
            let grants_clone = Arc::clone(&active_mcp_grants);

            let activate_tool = crate::tools::activate_tools::ActivateTools {
                stack_id:          None,
                mcp:               mcp_clone,
                active_mcp_grants: grants_clone,
            };

            let activate_tool = Arc::new(activate_tool);
            interface_tools.push(InterfaceTool {
                definition: activate_tools_tool_def(),
                handler: Arc::new(move |args| -> ToolFuture {
                    use crate::tools::Tool as _;
                    let tool = Arc::clone(&activate_tool);
                    Box::pin(async move {
                        tokio::task::spawn_blocking(move || tool.execute(args))
                            .await
                            .map_err(|e| anyhow::anyhow!("activate_tools task panicked: {e}"))?
                    })
                }),
            });
        }
        // ── End tool-group grant initialisation ─────────────────────────────────

        // Append RunContext system prompt fragments to the dynamic tail (not cached).
        let extra_system_dynamic = {
            let rc = self.run_context.read().await;
            let injected = rc.as_ref().and_then(|r| r.extra_system_prompt());
            match (extra_system_dynamic, injected) {
                (Some(e), Some(i)) => Some(format!("{e}\n\n{i}")),
                (Some(e), None)    => Some(e),
                (None,    Some(i)) => Some(i),
                (None,    None)    => None,
            }
        };

        let root_only_tool_names: Vec<String> = self.tools.root_agent_only_names();

        let memory_tools = self.memory_manager.tools().await;
        let image_tools  = Arc::clone(&self.image_generator_manager).tools().await;

        Ok(AgentRunConfig {
            agent_id:             self.agent_id.clone(),
            client_name:          key,
            depth:                0,
            base_tool_defs,
            config_tool_defs,
            extra_system,
            extra_system_dynamic,
            tail_reminder:        None,
            system_substitutions,
            interface_tools,
            memory_tools,
            image_tools,
            mcp:                  Arc::clone(&self.mcp),
            active_mcp_grants,
            root_only_tool_names,
        })
    }
}
