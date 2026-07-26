//! `SkaldToolSet` — the crate's `ToolSet` over Skald's tool surface (port of
//! `AgentRunConfig::all_tool_defs`, blueprint §10), plus the bridges that let
//! core-api tools and MCP tools run inside the crate's kernel (the "double
//! Tool trait" seam of phase 1: bridged, not re-exported).

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use agent_loop::activation::ToolRendering;
use agent_loop::async_trait;
use agent_loop::model::ModelInfo;
use agent_loop::tool::{
    MediaRef, RestartHint, Tool as LoopTool, ToolCtx, ToolExecution, ToolFailure,
    ToolOutput, ToolSet, Visibility,
};
use core_api::interface_tool::InterfaceTool;
use core_api::tool::{ExecutionOutcome as CoreOutcome, ToolExecutionState as CoreState};
use core_api::user_fs::UserFs;
use serde_json::Value;
use sqlx::SqlitePool;

use crate::mcp::McpProvider;
use crate::tools::tool_names::CONFIG_GROUP;

// ── Extension keys ───────────────────────────────────────────────────────────

/// The calling user's id — tools that address per-user external stores key on
/// it. Inserted by the host at TurnParams construction.
#[derive(Debug, Clone)]
pub struct CallerUserId(pub String);

/// Reads the `core_api::tool::ToolContext` pieces out of a `ToolCtx`:
/// owner pool + fs from the type-map, session id from the conversation.
fn core_tool_context(ctx: &ToolCtx) -> Result<core_api::tool::ToolContext, ToolFailure> {
    let pool = ctx.extensions.get::<SqlitePool>().ok_or_else(|| {
        ToolFailure::Failed("tool bridge: no SqlitePool in extensions".into())
    })?;
    let fs = ctx.extensions.get::<UserFs>().ok_or_else(|| {
        ToolFailure::Failed("tool bridge: no UserFs in extensions".into())
    })?;
    let user_id = ctx
        .extensions
        .get::<CallerUserId>()
        .map(|u| u.0.clone())
        .unwrap_or_default();
    let session_id = ctx
        .conversation
        .as_str()
        .strip_prefix("session:")
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or_default();
    Ok(core_api::tool::ToolContext { session_id, user_id, pool, fs })
}

/// Maps a core-api `ToolResult` to the crate's `ToolOutput`.
fn map_output(r: core_api::tool::ToolResult) -> ToolOutput {
    match r {
        core_api::tool::ToolResult::Text(s) => ToolOutput::Text(s),
        core_api::tool::ToolResult::Json(v) => ToolOutput::Json(v),
        core_api::tool::ToolResult::Media { text, media } => ToolOutput::Media {
            text,
            refs: media
                .iter()
                .map(|m| MediaRef { host_path: m.host_path.clone(), mime: m.mime.clone() })
                .collect(),
        },
    }
}

// ── BridgeExecution ──────────────────────────────────────────────────────────

/// Wraps a core-api `ToolExecution` as the crate's `ToolExecution` (the two
/// state machines are structurally identical).
struct BridgeExecution<'a> {
    inner: Box<dyn core_api::tool::ToolExecution + 'a>,
}

impl ToolExecution for BridgeExecution<'_> {
    fn state(&self) -> agent_loop::tool::ToolExecutionState {
        match self.inner.state() {
            CoreState::Pending | CoreState::AwaitingApproval | CoreState::Running => {
                agent_loop::tool::ToolExecutionState::Running
            }
            CoreState::Completed => agent_loop::tool::ToolExecutionState::Completed,
            CoreState::Failed    => agent_loop::tool::ToolExecutionState::Failed,
            CoreState::Cancelled | CoreState::Rejected => agent_loop::tool::ToolExecutionState::Cancelled,
        }
    }

    fn wait<'b>(&'b self) -> std::pin::Pin<Box<dyn std::future::Future<Output = agent_loop::tool::ExecutionOutcome> + Send + 'b>> {
        Box::pin(async move {
            match self.inner.wait().await {
                CoreOutcome::Completed(r) => agent_loop::tool::ExecutionOutcome::Completed(map_output(r)),
                CoreOutcome::Failed(e)    => agent_loop::tool::ExecutionOutcome::Failed(e),
                CoreOutcome::Cancelled    => agent_loop::tool::ExecutionOutcome::Cancelled,
            }
        })
    }

    fn stop<'b>(&'b self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'b>> {
        self.inner.stop()
    }
}

// ── CoreToolBridge ───────────────────────────────────────────────────────────

/// Runs a core-api tool (`crate::tools::Tool`) inside the crate's kernel:
/// context from the type-map, execution bridged (kill/teardown preserved —
/// `execute_cmd`'s reaper keeps working through `stop`).
pub struct CoreToolBridge {
    inner: Arc<dyn crate::tools::Tool>,
}

impl CoreToolBridge {
    pub fn new(inner: Arc<dyn crate::tools::Tool>) -> Self { Self { inner } }
}

#[async_trait]
impl LoopTool for CoreToolBridge {
    fn name(&self) -> &str { self.inner.name() }

    fn definition(&self) -> Value { self.inner.openai_definition() }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        // Same path as `start`, driven to completion without a cancel token.
        let exec = self.start(args, ctx);
        match exec.wait().await {
            agent_loop::tool::ExecutionOutcome::Completed(out) => Ok(out),
            agent_loop::tool::ExecutionOutcome::Failed(e)      => Err(ToolFailure::Failed(e)),
            agent_loop::tool::ExecutionOutcome::Cancelled |
            agent_loop::tool::ExecutionOutcome::Suspended      => {
                Err(ToolFailure::Failed("tool execution interrupted".into()))
            }
        }
    }

    fn start<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> Box<dyn ToolExecution + 'a> {
        match core_tool_context(ctx) {
            Ok(tool_ctx) => Box::new(BridgeExecution { inner: self.inner.run_with(&tool_ctx, args) }),
            Err(e)       => Box::new(agent_loop::tool::SimpleExecution::new(Box::pin(async move { Err(e) }))),
        }
    }

    fn restart_hint(&self) -> RestartHint {
        // D7: shell commands are not idempotent — never re-run them on restart.
        if self.inner.name() == "execute_cmd" {
            RestartHint::MarkInterrupted
        } else {
            RestartHint::ReExecute
        }
    }

    fn visibility(&self) -> Visibility {
        if self.inner.root_agent_only() {
            Visibility::RootOnly
        } else if self.inner.sub_agents_only() {
            Visibility::SubAgentsOnly
        } else if self.inner.interactive_only() {
            Visibility::InteractiveOnly
        } else {
            Visibility::Always
        }
    }
}

// ── McpToolBridge ────────────────────────────────────────────────────────────

/// Runs one MCP tool (`mcp__server__tool`) inside the crate's kernel.
pub struct McpToolBridge {
    mcp:        Arc<dyn McpProvider>,
    server:     String,
    tool:       String,
    definition: Value,
}

impl McpToolBridge {
    pub fn new(mcp: Arc<dyn McpProvider>, server: impl Into<String>, tool: impl Into<String>, definition: Value) -> Self {
        Self { mcp, server: server.into(), tool: tool.into(), definition }
    }
}

#[async_trait]
impl LoopTool for McpToolBridge {
    fn name(&self) -> &str { self.definition["function"]["name"].as_str().unwrap_or("") }

    fn definition(&self) -> Value { self.definition.clone() }

    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        match self.mcp.call(&self.server, &self.tool, args).await {
            Ok(r)  => Ok(map_output(r)),
            Err(e) => Err(ToolFailure::Failed(e.to_string())),
        }
    }
}

// ── SkaldToolSet ─────────────────────────────────────────────────────────────

/// The per-turn tool set: base built-ins + MCP grants + the lazy `config`
/// group + memory/image/interface tools, rendered per the model's
/// `ToolRendering` (D15). `defs` is re-read at every round/attempt — grants
/// activated at round N are visible at round N+1 for free.
pub struct SkaldToolSet {
    base_defs:        Vec<Value>,
    config_defs:      Vec<Value>,
    mcp:              Arc<dyn McpProvider>,
    grants:           Arc<RwLock<HashSet<String>>>,
    memory_tools:     Vec<Arc<dyn crate::tools::Tool>>,
    image_tools:      Vec<Arc<dyn crate::tools::Tool>>,
    /// Crate-native tools (ActivateToolsTool, aliases) — returned as-is.
    interface_tools:  Vec<InterfaceTool>,
    /// Core tools available for execution by name (the find() side).
    core_tools:       Vec<Arc<dyn crate::tools::Tool>>,
    /// Extra crate-native tools for find() (bridge-free).
    native_tools:     Vec<Arc<dyn LoopTool>>,
}

impl SkaldToolSet {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_defs:       Vec<Value>,
        config_defs:     Vec<Value>,
        mcp:             Arc<dyn McpProvider>,
        grants:          Arc<RwLock<HashSet<String>>>,
        memory_tools:    Vec<Arc<dyn crate::tools::Tool>>,
        image_tools:     Vec<Arc<dyn crate::tools::Tool>>,
        interface_tools: Vec<InterfaceTool>,
        core_tools:      Vec<Arc<dyn crate::tools::Tool>>,
    ) -> Self {
        Self {
            base_defs,
            config_defs,
            mcp,
            grants,
            memory_tools,
            image_tools,
            interface_tools,
            core_tools,
            native_tools: Vec::new(),
        }
    }

    pub fn with_native(mut self, tool: Arc<dyn LoopTool>) -> Self {
        self.native_tools.push(tool);
        self
    }
}

/// Tags an OpenAI tool definition as deferred (Anthropic tool search).
fn deferred(mut def: Value) -> Value {
    def["defer_loading"] = Value::Bool(true);
    def
}

impl ToolSet for SkaldToolSet {
    fn defs(&self, model: &ModelInfo) -> Vec<Value> {
        let mut defs = self.base_defs.clone();

        match model.tool_rendering {
            // Declare EVERY accessible MCP tool + the config group as
            // `defer_loading:true` — a stable, cache-safe set.
            ToolRendering::DeferredToolReference => {
                defs.extend(self.mcp.tools().iter().map(|t| deferred(t.to_openai_definition())));
                defs.extend(self.config_defs.iter().cloned().map(deferred));
            }
            // Activated tools are injected as `system`+`tools` messages by the
            // assembler — NOT in the top-level array.
            ToolRendering::SystemToolBlock => {}
            ToolRendering::Inline => {
                let granted: HashSet<String> = self.grants.read().map(|g| g.clone()).unwrap_or_default();
                let servers: Vec<String> = granted
                    .iter()
                    .filter(|n| n.as_str() != CONFIG_GROUP)
                    .cloned()
                    .collect();
                if !servers.is_empty() {
                    defs.extend(self.mcp.tools_for(&servers).iter().map(|t| t.to_openai_definition()));
                }
                if granted.contains(CONFIG_GROUP) {
                    defs.extend(self.config_defs.iter().cloned());
                }
            }
        }

        defs.extend(self.memory_tools.iter().map(|t| t.openai_definition()));
        defs.extend(self.image_tools.iter().map(|t| t.openai_definition()));
        defs.extend(self.interface_tools.iter().map(|t| t.definition.clone()));
        defs.extend(self.native_tools.iter().map(|t| t.definition()));
        defs
    }

    fn find(&self, name: &str) -> Option<Arc<dyn LoopTool>> {
        if let Some(t) = self.native_tools.iter().find(|t| t.name() == name) {
            return Some(t.clone());
        }
        if let Some(t) = self.core_tools.iter().find(|t| t.name() == name) {
            return Some(Arc::new(CoreToolBridge::new(t.clone())));
        }
        if let Some(t) = self.memory_tools.iter().find(|t| t.name() == name) {
            return Some(Arc::new(CoreToolBridge::new(t.clone())));
        }
        if let Some(t) = self.image_tools.iter().find(|t| t.name() == name) {
            return Some(Arc::new(CoreToolBridge::new(t.clone())));
        }
        // MCP names are `mcp__<server>__<tool>`.
        if let Some((server, tool)) = crate::mcp::parse_mcp_tool_name(name) {
            let def = self
                .mcp
                .tools_for(&[server.to_string()])
                .into_iter()
                .find(|t| t.name == tool)
                .map(|t| t.to_openai_definition());
            if let Some(def) = def {
                return Some(Arc::new(McpToolBridge::new(self.mcp.clone(), server, tool, def)));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use mcp_client::McpTool;

    use crate::tools::ToolResult;

    fn fake_mcp(server: &str, tool_names: &[&str]) -> Arc<dyn McpProvider> {
        struct Fake(Vec<McpTool>);
        #[async_trait::async_trait]
        impl McpProvider for Fake {
            fn tools(&self) -> Vec<McpTool> { self.0.clone() }
            fn tools_for(&self, names: &[String]) -> Vec<McpTool> {
                self.0.iter().filter(|t| names.contains(&t.server_name)).cloned().collect()
            }
            fn server_descriptions(&self) -> HashMap<String, Option<String>> { HashMap::new() }
            fn server_infos(&self) -> Vec<Value> { Vec::new() }
            fn tool_display_name(&self, _s: &str, _t: &str) -> Option<String> { None }
            async fn call(&self, _s: &str, _t: &str, _a: Value) -> anyhow::Result<ToolResult> {
                unimplemented!()
            }
        }
        Arc::new(Fake(
            tool_names
                .iter()
                .map(|t| McpTool {
                    server_name:   server.to_string(),
                    name:          t.to_string(),
                    description:   String::new(),
                    input_schema:  serde_json::json!({"type":"object"}),
                    title:         None,
                    output_schema: None,
                    annotations:   None,
                    task_support:  None,
                })
                .collect(),
        ))
    }

    fn set(grants: &[&str]) -> Arc<RwLock<HashSet<String>>> {
        Arc::new(RwLock::new(grants.iter().map(|s| s.to_string()).collect()))
    }

    fn toolset(grants: Arc<RwLock<HashSet<String>>>) -> SkaldToolSet {
        SkaldToolSet::new(
            vec![serde_json::json!({"type":"function","function":{"name":"read_file","parameters":{}}})],
            vec![serde_json::json!({"type":"function","function":{"name":"cron_list","parameters":{}}})],
            fake_mcp("gmail", &["send"]),
            grants,
            vec![],
            vec![],
            vec![],
            vec![],
        )
    }

    #[test]
    fn inline_renders_only_granted_groups() {
        let ts = toolset(set(&[]));
        let defs = ts.defs(&ModelInfo::default());
        let names: Vec<&str> = defs.iter().filter_map(|d| d["function"]["name"].as_str()).collect();
        assert_eq!(names, ["read_file"]);

        let ts = toolset(set(&["gmail", CONFIG_GROUP]));
        let defs = ts.defs(&ModelInfo::default());
        let names: Vec<&str> = defs.iter().filter_map(|d| d["function"]["name"].as_str()).collect();
        assert!(names.contains(&"mcp__gmail__send"), "{names:?}");
        assert!(names.contains(&"cron_list"));
    }

    #[test]
    fn deferred_declares_everything_tagged() {
        let ts = toolset(set(&[]));
        let info = ModelInfo { tool_rendering: ToolRendering::DeferredToolReference, ..Default::default() };
        let defs = ts.defs(&info);
        let gmail = defs.iter().find(|d| d["function"]["name"].as_str() == Some("mcp__gmail__send")).unwrap();
        assert_eq!(gmail["defer_loading"], serde_json::json!(true));
        let base = defs.iter().find(|d| d["function"]["name"].as_str() == Some("read_file")).unwrap();
        assert!(base.get("defer_loading").is_none());
    }

    #[test]
    fn system_tool_block_keeps_array_stable() {
        let ts = toolset(set(&["gmail"]));
        let info = ModelInfo { tool_rendering: ToolRendering::SystemToolBlock, ..Default::default() };
        let defs = ts.defs(&info);
        let names: Vec<&str> = defs.iter().filter_map(|d| d["function"]["name"].as_str()).collect();
        assert_eq!(names, ["read_file"], "activated tools must NOT be in the array in Kimi mode");
    }

    #[test]
    fn find_bridges_mcp_names() {
        let ts = toolset(set(&["gmail"]));
        let t = ts.find("mcp__gmail__send").expect("mcp tool not bridged");
        assert_eq!(t.definition()["function"]["name"], serde_json::json!("mcp__gmail__send"));
        assert!(ts.find("mcp__gmail__nope").is_none());
        assert!(ts.find("unknown_tool").is_none());
    }
}
