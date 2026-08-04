use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::agents;
use crate::cron::TaskManager;
use crate::plugin::PluginManager;
use crate::tools::{Tool, ToolContext, ToolDescriptionLength, ToolExecution};

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
    plugins: Arc<PluginManager>,
    cron:    Arc<TaskManager>,
    /// The registry (`system.db`), for the `mcp` report's instance-wide half:
    /// the catalog, the global connectors and the caller's capabilities. Captured
    /// at construction because it is the same file for everyone — the *owner*
    /// half arrives per call, on the `ToolContext`.
    registry: Arc<SqlitePool>,
}

impl ListItems {
    pub fn new(
        plugins:  Arc<PluginManager>,
        cron:     Arc<TaskManager>,
        registry: Arc<SqlitePool>,
    ) -> Self {
        Self { plugins, cron, registry }
    }
}

impl Tool for ListItems {
    fn name(&self) -> &str { "list_items" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Introspection }

    fn description(&self) -> &str {
        "List configured items of a given type. Pass `type`:\n\
         • `plugins` — plugins with id, name, description, enabled flag (persisted), and running flag (live).\n\
         • `cron` — scheduled tasks/cron jobs with id, title, cron expression, agent_id, enabled, kind, last/next run.\n\
         • `agents` — sub-agents available to delegate to (id, name, description, optional `instructions` on how to call the agent well, optional client). Do NOT invoke the `main` agent.\n\
         • `mcp` — MCP servers, which users call \"Connectors\": which ones are already loaded into this session, which are ready for `activate_tools`, which are installed but unusable and why, and which the user could still activate. Read this before assuming a connector is missing.\n\
         To list stored secret names use `list_secrets` instead."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["type"],
            "properties": {
                "type": {
                    "type": "string",
                    "enum": ["plugins", "cron", "agents", "mcp"],
                    "description": "Which kind of item to list."
                }
            }
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let kind = args["type"].as_str().unwrap_or("?");
        format!("list {kind}")
    }

    /// `mcp` is the one type that needs the caller: which connectors are theirs,
    /// which are loaded into *this* session, and what their role may do. The
    /// other three are instance-wide and stay on the context-free `execute`.
    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        if args["type"].as_str() != Some("mcp") {
            return self.run(args);
        }
        let registry   = Arc::clone(&self.registry);
        let owner      = Arc::clone(&ctx.pool);
        let user_id    = ctx.user_id.clone();
        let session_id = ctx.session_id;
        let mcp        = ctx.mcp.clone();
        Box::new(crate::tools::SimpleExecution::new(Box::pin(async move {
            let report = crate::tools::mcp_report::build(
                &registry,
                &owner,
                &user_id,
                session_id,
                mcp.as_deref(),
            )
            .await?;
            Ok(crate::tools::ToolResult::Json(report))
        })))
    }

    fn execute(&self, args: Value) -> Result<String> {
        let kind = args["type"].as_str()
            .ok_or_else(|| anyhow::anyhow!("list_items: missing required argument `type`"))?;

        match kind {
            // Reached only through the context-free `execute` (no caller, so no
            // report to build) — `run_with` intercepts the real call path.
            "mcp" => anyhow::bail!(
                "list_items: type `mcp` needs a session context and was called without one"
            ),
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
            other => anyhow::bail!("list_items: unknown type `{other}` (expected one of: plugins, cron, agents, mcp)"),
        }
    }
}
