//! `AgentSystemContext` — Skald's agent prompt as a `SystemContextSource`
//! (the static half of the old `MessageBuilder::build`, blueprint §10):
//! AGENT.md + `inject_memory` files + skills index + `extra_system` +
//! `__MCP_LIST__` / `__SHARED_FOLDERS__` / `__USER_PROFILE__` / custom
//! substitutions. The dynamic tail (Honcho memory, per-turn overrides) rides
//! as `dynamic_tail`; the datetime line and scratchpad stay assembler-side.

use std::collections::HashMap;
use std::sync::Arc;

use agent_loop::context::{SystemContext, SystemContextSource, TurnInfo};
use sqlx::SqlitePool;

use crate::mcp::McpProvider;

/// Registry of installed skills, relative to Skald's process cwd. Injected
/// into agents that have `inject_skills` enabled (the default).
const SKILLS_INDEX_PATH: &str = "skills/index.md";

/// The static system content of one agent, resolved per turn.
pub struct AgentSystemContext {
    pub agent_id:      String,
    /// Static extra context (interface formatting rules, e.g. Telegram HTML).
    pub extra_static:  Option<String>,
    /// Dynamic extra context (Honcho memory merged with per-turn overrides),
    /// emitted as the dynamic tail.
    pub extra_dynamic: Option<String>,
    pub tail_reminder: Option<String>,
    pub substitutions: HashMap<String, String>,
    /// Owner pool (`user-memory/` notes).
    pub pool:          Arc<SqlitePool>,
    /// Shared pool (`shared-memory/`, shared folders, user profile).
    pub shared_pool:   Arc<SqlitePool>,
    pub user_id:       String,
    pub mcp:           Arc<dyn McpProvider>,
    /// Project root for `__PROJECT_ROOT__` expansion in `inject_memory`.
    pub project_root:  Option<String>,
}

#[agent_loop::async_trait]
impl SystemContextSource for AgentSystemContext {
    async fn system_context(&self, _turn: &TurnInfo) -> agent_loop::Result<SystemContext> {
        let mut static_content = crate::agents::load_prompt(&self.agent_id)?;

        let meta = crate::agents::load_meta(&self.agent_id)?;
        if !meta.inject_memory.is_empty() {
            static_content.push_str(
                "\n\n---\nThe following memory files have been loaded automatically. \
                 You can edit them with `edit_file` or `write_file` using the path shown.\n"
            );
            for mem_path in &meta.inject_memory {
                let (content, display) = self.load_inject_memory(mem_path).await;
                match content {
                    Some(c) => static_content.push_str(&format!(
                        "\n<memory_file path=\"{display}\">\n{c}\n</memory_file>\n"
                    )),
                    None => static_content.push_str(&format!(
                        "\n<memory_file path=\"{display}\">\n(file not created yet)\n</memory_file>\n"
                    )),
                }
            }
        }

        // Skills index — injected unless the agent opts out. Skipped silently
        // when no skills are installed.
        if meta.inject_skills {
            let (abs, display) = self.resolve_memory_path(SKILLS_INDEX_PATH);
            if let Ok(c) = tokio::fs::read_to_string(&abs).await {
                static_content.push_str(&format!(
                    "\n\n---\nInstalled skills you can use (read the linked `SKILL.md` before running a skill):\n\
                     \n<skills_index path=\"{display}\">\n{c}\n</skills_index>\n"
                ));
            }
        }

        if let Some(extra) = &self.extra_static {
            static_content.push_str("\n\n---\n");
            static_content.push_str(extra);
        }

        if static_content.contains("__MCP_LIST__") {
            static_content = static_content.replace("__MCP_LIST__", &self.render_mcp_list());
        }
        if static_content.contains("__SHARED_FOLDERS__") {
            static_content = static_content.replace(
                "__SHARED_FOLDERS__",
                &crate::session::handler::message_builder::render_shared_folders_section(
                    &self.shared_pool,
                    &self.user_id,
                )
                .await?,
            );
        }
        if static_content.contains("__USER_PROFILE__") {
            static_content = static_content.replace(
                "__USER_PROFILE__",
                &crate::session::handler::message_builder::render_user_profile_section(
                    &self.shared_pool,
                    &self.user_id,
                )
                .await?,
            );
        }

        for (key, value) in &self.substitutions {
            let sentinel = format!("__{key}__");
            if static_content.contains(sentinel.as_str()) {
                static_content = static_content.replace(sentinel.as_str(), value);
            }
        }

        Ok(SystemContext {
            base:          static_content,
            extra_static:  Vec::new(),
            dynamic_tail:  self.extra_dynamic.clone().into_iter().collect(),
            tail_reminder: self.tail_reminder.clone(),
        })
    }
}

impl AgentSystemContext {
    /// Loads an `inject_memory` entry, returning `(content, display_path)`.
    /// Virtual memory paths read from SQLite; everything else is a disk read.
    async fn load_inject_memory(&self, mem_path: &str) -> (Option<String>, String) {
        use crate::tools::fs::{MemScope, classify_memory};
        if let Some(m) = classify_memory(mem_path) {
            let pool = match m.scope {
                MemScope::User   => &self.pool,
                MemScope::Shared => &self.shared_pool,
            };
            let content = crate::db::memory_docs::get(pool, &m.rel)
                .await.ok().flatten().map(|d| d.content);
            return (content, mem_path.to_string());
        }
        let (abs, display) = self.resolve_memory_path(mem_path);
        (tokio::fs::read_to_string(&abs).await.ok(), display)
    }

    fn resolve_memory_path(&self, mem_path: &str) -> (std::path::PathBuf, String) {
        let display = if mem_path.contains("__PROJECT_ROOT__") {
            match &self.project_root {
                Some(root) => mem_path.replace("__PROJECT_ROOT__", root),
                None => {
                    tracing::warn!(
                        mem_path,
                        "inject_memory entry references __PROJECT_ROOT__ but this session has no project root; skipping"
                    );
                    return (std::path::PathBuf::from(mem_path), mem_path.to_string());
                }
            }
        } else {
            mem_path.to_string()
        };
        let abs = crate::tools::fs::resolve(&display)
            .unwrap_or_else(|_| std::path::PathBuf::from(&display));
        (abs, display)
    }

    /// The **static** catalogue of loadable MCP servers (identical regardless
    /// of which are active — cache-prefix stability).
    fn render_mcp_list(&self) -> String {
        let all_servers: std::collections::BTreeSet<String> = self.mcp.tools()
            .into_iter()
            .map(|t| t.server_name)
            .collect();

        if all_servers.is_empty() {
            return String::new();
        }

        let descriptions = self.mcp.server_descriptions();

        let mut out = String::from(
            "## MCP servers\n\nConnectors you can load with `activate_tools([\"name\"])`. \
             Once loaded, a server's tools are callable as `mcp__<name>__<tool>`:\n\n",
        );
        out.push_str("| Server | Description |\n|--------|-------------|\n");
        for name in &all_servers {
            let desc = descriptions.get(name)
                .and_then(|d| d.as_deref())
                .unwrap_or("—");
            out.push_str(&format!("| `{name}` | {desc} |\n"));
        }
        out
    }
}
