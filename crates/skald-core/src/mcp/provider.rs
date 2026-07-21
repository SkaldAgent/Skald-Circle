//! The MCP tool surface a session sees, behind one trait.
//!
//! A logged-in user's tools are the union of two runtimes (blueprint §7): the
//! access-filtered GLOBAL runtime (host, shared) and their own PER-USER runtime
//! (in their container). [`McpProvider`] is the seam the session code talks to,
//! so the round-loop (`all_tool_defs`, `render_mcp_list`, `ActivateTools`) never
//! has to know which runtime owns a server. [`McpManager`] implements it directly
//! (used as-is for the inert ownerless bundle); [`UserMcpView`] implements it as
//! the union.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use crate::tools::ToolResult;

use super::{McpManager, McpTool};

#[async_trait]
pub trait McpProvider: Send + Sync {
    fn tools(&self) -> Vec<McpTool>;
    fn tools_for(&self, names: &[String]) -> Vec<McpTool>;
    fn server_descriptions(&self) -> HashMap<String, Option<String>>;
    fn server_infos(&self) -> Vec<Value>;
    /// Best friendly name for a `server`/`tool` pair for the chat card (manifest
    /// override > live MCP `title` > `None`, the caller then prettifies the raw
    /// name). Routed to whichever runtime owns the server.
    fn tool_display_name(&self, server: &str, tool: &str) -> Option<String>;
    async fn call(&self, server: &str, tool: &str, args: Value) -> Result<ToolResult>;
}

#[async_trait]
impl McpProvider for McpManager {
    fn tools(&self) -> Vec<McpTool> { McpManager::tools(self) }
    fn tools_for(&self, names: &[String]) -> Vec<McpTool> { McpManager::tools_for(self, names) }
    fn server_descriptions(&self) -> HashMap<String, Option<String>> { McpManager::server_descriptions(self) }
    fn server_infos(&self) -> Vec<Value> { McpManager::server_infos(self) }
    fn tool_display_name(&self, server: &str, tool: &str) -> Option<String> {
        McpManager::tool_display_name(self, server, tool)
    }
    async fn call(&self, server: &str, tool: &str, args: Value) -> Result<ToolResult> {
        McpManager::call(self, server, tool, args).await
    }
}

/// One logged-in user's MCP view: the access-filtered global runtime unioned with
/// their per-user container runtime. A per-user server wins on a name collision
/// (which activation prevents anyway — see the uniqueness check at activation).
pub struct UserMcpView {
    pub global: Arc<McpManager>,
    pub user:   Arc<McpManager>,
    /// Names of the global servers this user may use — a snapshot of
    /// `mcp_global_access`, captured when the user's context is built.
    pub accessible_global: HashSet<String>,
}

impl UserMcpView {
    fn accessible_names(&self) -> Vec<String> {
        self.accessible_global.iter().cloned().collect()
    }
}

#[async_trait]
impl McpProvider for UserMcpView {
    fn tools(&self) -> Vec<McpTool> {
        let mut out = self.global.tools_for(&self.accessible_names());
        out.extend(self.user.tools());
        out
    }

    fn tools_for(&self, names: &[String]) -> Vec<McpTool> {
        // A granted name belongs to exactly one runtime (unique per user); route
        // the accessible-global ones to the global runtime and the rest to the
        // per-user one, which filters to its own server map.
        let global_names: Vec<String> = names.iter()
            .filter(|n| self.accessible_global.contains(*n))
            .cloned()
            .collect();
        let mut out = self.global.tools_for(&global_names);
        out.extend(self.user.tools_for(names));
        out
    }

    fn server_descriptions(&self) -> HashMap<String, Option<String>> {
        let mut m: HashMap<String, Option<String>> = self.global.server_descriptions()
            .into_iter()
            .filter(|(name, _)| self.accessible_global.contains(name))
            .collect();
        m.extend(self.user.server_descriptions());
        m
    }

    fn server_infos(&self) -> Vec<Value> {
        let mut v: Vec<Value> = self.global.server_infos()
            .into_iter()
            .filter(|info| info["name"].as_str()
                .map(|n| self.accessible_global.contains(n))
                .unwrap_or(false))
            .collect();
        v.extend(self.user.server_infos());
        v
    }

    fn tool_display_name(&self, server: &str, tool: &str) -> Option<String> {
        if self.accessible_global.contains(server) {
            self.global.tool_display_name(server, tool)
        } else {
            self.user.tool_display_name(server, tool)
        }
    }

    async fn call(&self, server: &str, tool: &str, args: Value) -> Result<ToolResult> {
        if self.accessible_global.contains(server) {
            self.global.call(server, tool, args).await
        } else {
            // A per-user server, or an unknown/forbidden one — the per-user
            // runtime returns a "not found" error for the latter.
            self.user.call(server, tool, args).await
        }
    }
}
