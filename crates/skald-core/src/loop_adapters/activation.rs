//! DTL activation adapters (blueprint D15): the crate owns the wire protocol,
//! Skald owns the catalog (MCP servers + the reserved `config` group) and the
//! persistence (`activated_tools`, anchored at the triggering message).

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use agent_loop::activation::{Activation, ActivationSource, ToolActivator};
use agent_loop::ids::{FrameId, MessageId};
use agent_loop::tool::{ToolCtx, ToolFailure};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::db::{
    activated_tools, chat_llm_tools, mcp_catalog, mcp_catalog_access, mcp_global_access,
    mcp_global_servers, mcp_user_servers,
};
use crate::mcp::McpProvider;
use crate::tools::tool_names::CONFIG_GROUP;

// ── ActivationSource ─────────────────────────────────────────────────────────

/// Reads the durable activations of one scope (root session or sub-agent
/// frame) and resolves them to OpenAI tool defs for the assembler's DTL
/// injection: which tool definitions an activation resolves to.
pub struct SkaldActivationSource {
    pool:        Arc<SqlitePool>,
    mcp:         Arc<dyn McpProvider>,
    config_defs: Arc<Vec<Value>>,
    session_id:  i64,
    /// `None` = root (session scope); `Some(stack_id)` = sub-agent frame.
    stack:       Option<i64>,
}

impl SkaldActivationSource {
    pub fn new(
        pool:        Arc<SqlitePool>,
        mcp:         Arc<dyn McpProvider>,
        config_defs: Arc<Vec<Value>>,
        session_id:  i64,
        stack:       Option<i64>,
    ) -> Self {
        Self { pool, mcp, config_defs, session_id, stack }
    }
}

#[agent_loop::async_trait]
impl ActivationSource for SkaldActivationSource {
    async fn activations(&self, _frame: FrameId) -> agent_loop::Result<Vec<Activation>> {
        let rows = activated_tools::list_active_at(&self.pool, self.session_id, self.stack, i64::MAX).await?;

        // Group by anchor, dedup tool names per anchor (a server may reappear).
        let mut out: Vec<Activation> = Vec::new();
        for row in rows {
            let defs: Vec<Value> = if row.kind == "builtin" && row.ref_ == CONFIG_GROUP {
                self.config_defs.as_ref().clone()
            } else {
                self.mcp
                    .tools_for(std::slice::from_ref(&row.ref_))
                    .iter()
                    .map(|t| t.to_openai_definition())
                    .collect()
            };
            let anchor = MessageId(row.message_id);
            match out.iter_mut().find(|a| a.anchor == anchor) {
                Some(existing) => {
                    for d in defs {
                        let name = d["function"]["name"].as_str().unwrap_or("");
                        if !existing.defs.iter().any(|e| e["function"]["name"].as_str() == Some(name)) {
                            existing.defs.push(d);
                        }
                    }
                }
                None => out.push(Activation { anchor, defs }),
            }
        }
        Ok(out)
    }
}

// ── ToolActivator ────────────────────────────────────────────────────────────

/// What a requested group resolved to. Only [`Status::Activated`] grants
/// anything: every other state means the group's tools cannot appear in this
/// session, and saying otherwise would have the model call `mcp__x__…` a round
/// later and fail there instead of here.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Running (or the built-in `config` group) — granted and persisted.
    Activated,
    /// The user activated the connector but never finished signing in
    /// (`auth_state != 'ready'`), or they disabled it.
    NeedsLogin,
    /// Installed and disabled, or in the catalog and never activated: the USER
    /// can fix it from Connectors.
    NotActivated,
    /// Exists but the ADMIN must act (a global connector not enabled/granted, a
    /// catalog entry this user is not authorized for).
    NotAuthorized,
    /// Meant to be running and isn't — a start/connect failure, not a
    /// configuration one. Transient.
    Unavailable,
    /// No such connector anywhere.
    Unknown,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Activated     => "activated",
            Status::NeedsLogin    => "needs_login",
            Status::NotActivated  => "not_activated",
            Status::NotAuthorized => "not_authorized",
            Status::Unavailable   => "unavailable",
            Status::Unknown       => "unknown",
        }
    }
}

/// One group's outcome, rendered as one JSON object. The shape is identical in
/// success and failure: `status` is what the model branches on, `message` is
/// what it relays to the user (the audience is non-technical — see `docs/`).
struct GroupReport {
    status:      Status,
    tool_prefix: Option<String>,
    tool_count:  usize,
    description: Option<String>,
    message:     String,
}

impl GroupReport {
    fn to_json(&self) -> Value {
        json!({
            "status":      self.status.as_str(),
            "tool_prefix": self.tool_prefix,
            "tool_count":  self.tool_count,
            "description": self.description,
            "message":     self.message,
        })
    }
}

/// Backend of the crate's shipped `activate_tools` tool: resolves each group
/// against the runtime and the connector tables, updates the in-memory grant
/// set **immediately** for the ones that resolved (next round sees the tools),
/// and persists those activations anchored at the triggering assistant message
/// (derived from the call's `chat_llm_tools` row).
///
/// A group that cannot be activated is diagnosed rather than accepted: it
/// touches neither the grant set nor `activated_tools`, and the report says
/// which of the connector states (§7/§15) it is in and who can fix it.
pub struct SkaldToolActivator {
    /// Owner pool — `mcp_user_servers` (this user's activations).
    pool:        Arc<SqlitePool>,
    /// Registry pool — `mcp_catalog`, `mcp_global_servers` and the access grants.
    shared_pool: Arc<SqlitePool>,
    user_id:     String,
    mcp:         Arc<dyn McpProvider>,
    /// The reserved `config` group's defs, for its tool count.
    config_defs: Arc<Vec<Value>>,
    grants:      Arc<RwLock<HashSet<String>>>,
    session_id:  i64,
    stack:       Option<i64>,
}

impl SkaldToolActivator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pool:        Arc<SqlitePool>,
        shared_pool: Arc<SqlitePool>,
        user_id:     String,
        mcp:         Arc<dyn McpProvider>,
        config_defs: Arc<Vec<Value>>,
        grants:      Arc<RwLock<HashSet<String>>>,
        session_id:  i64,
        stack:       Option<i64>,
    ) -> Self {
        Self { pool, shared_pool, user_id, mcp, config_defs, grants, session_id, stack }
    }

    /// Where the activated tools land, for the confirmation message.
    fn scope_label(&self) -> &'static str {
        match self.stack {
            None    => "this session",
            Some(_) => "this sub-agent frame",
        }
    }

    /// Resolves one group name to its state. A diagnosis query that fails is
    /// logged and treated as "no row" — a broken lookup must not turn into a
    /// false claim about the connector.
    async fn resolve(&self, name: &str) -> GroupReport {
        if name == CONFIG_GROUP {
            return GroupReport {
                status:      Status::Activated,
                tool_prefix: None,
                tool_count:  self.config_defs.len(),
                description: Some(
                    "Built-in system-configuration tools: connectors, plugins, scheduled jobs, secrets, installing and deleting skills."
                        .into(),
                ),
                message:     format!("Tools are in context for {} from the next round.", self.scope_label()),
            };
        }

        // Running in this user's view (global ∪ per-user, already access-filtered).
        let key = [name.to_string()];
        let running = self.mcp.tools_for(&key);
        if !running.is_empty() {
            return GroupReport {
                status:      Status::Activated,
                tool_prefix: Some(format!("mcp__{name}__")),
                tool_count:  running.len(),
                description: self.mcp.server_descriptions().get(name).cloned().flatten(),
                message:     format!("Tools are in context for {} from the next round.", self.scope_label()),
            };
        }

        // Not running — diagnose, from the user's own activations outward.
        if let Some(row) = self
            .lookup(mcp_user_servers::get_by_name(&self.pool, name).await, "mcp_user_servers", name)
            .flatten()
        {
            let description = self.catalog_description(row.catalog_name.as_deref()).await;
            if !row.enabled {
                return GroupReport {
                    status: Status::NotActivated,
                    tool_prefix: None,
                    tool_count: 0,
                    description,
                    message: format!(
                        "The `{name}` connector is set up for this user but switched off. \
                         Tell the user to re-enable it in Connectors."
                    ),
                };
            }
            if row.auth_state != "ready" {
                let how = self.login_hint(row.catalog_name.as_deref()).await;
                return GroupReport {
                    status: Status::NeedsLogin,
                    tool_prefix: None,
                    tool_count: 0,
                    description,
                    message: format!(
                        "The `{name}` connector is installed but the sign-in was never completed. \
                         Tell the user to open Connectors → {name} and {how}."
                    ),
                };
            }
            return GroupReport {
                status: Status::Unavailable,
                tool_prefix: None,
                tool_count: 0,
                description,
                message: format!(
                    "The `{name}` connector is set up and enabled but its server is not running \
                     right now — it failed to start. This is temporary and not something the user \
                     can fix from the interface."
                ),
            };
        }

        if let Some(row) = self
            .lookup(mcp_global_servers::get_by_name(&self.shared_pool, name).await, "mcp_global_servers", name)
            .flatten()
        {
            let description = row.description.clone();
            if !row.enabled {
                return GroupReport {
                    status: Status::NotAuthorized,
                    tool_prefix: None,
                    tool_count: 0,
                    description,
                    message: format!(
                        "`{name}` is a shared connector that the administrator has disabled. \
                         Only an administrator can turn it back on."
                    ),
                };
            }
            let granted = self
                .lookup(mcp_global_access::effective_access(&self.shared_pool, row.id, &self.user_id).await, "mcp_global_access", name)
                .unwrap_or(false);
            if !granted {
                return GroupReport {
                    status: Status::NotAuthorized,
                    tool_prefix: None,
                    tool_count: 0,
                    description,
                    message: format!(
                        "`{name}` is a shared connector this user has not been given access to. \
                         Only an administrator can grant it."
                    ),
                };
            }
            return GroupReport {
                status: Status::Unavailable,
                tool_prefix: None,
                tool_count: 0,
                description,
                message: format!(
                    "`{name}` is enabled and granted but its server is not running right now — \
                     it failed to start. This is temporary and not something the user can fix."
                ),
            };
        }

        if let Some(row) = self
            .lookup(mcp_catalog::get_by_name(&self.shared_pool, name).await, "mcp_catalog", name)
            .flatten()
        {
            let description = row.description.clone();
            if row.scope == "global" {
                return GroupReport {
                    status: Status::NotAuthorized,
                    tool_prefix: None,
                    tool_count: 0,
                    description,
                    message: format!(
                        "`{name}` is installed but not switched on as a shared connector. \
                         Only an administrator can enable it."
                    ),
                };
            }
            let authorized = self
                .lookup(mcp_catalog_access::effective_access(&self.shared_pool, name, &self.user_id).await, "mcp_catalog_access", name)
                .unwrap_or(false);
            return if authorized {
                GroupReport {
                    status: Status::NotActivated,
                    tool_prefix: None,
                    tool_count: 0,
                    description,
                    message: format!(
                        "`{name}` is available to this user but not activated yet. \
                         Tell the user they can activate it in Connectors, then ask again."
                    ),
                }
            } else {
                GroupReport {
                    status: Status::NotAuthorized,
                    tool_prefix: None,
                    tool_count: 0,
                    description,
                    message: format!(
                        "`{name}` is installed on this instance but this user is not authorized \
                         to activate it. Only an administrator can authorize them."
                    ),
                }
            };
        }

        GroupReport {
            status:      Status::Unknown,
            tool_prefix: None,
            tool_count:  0,
            description: None,
            message:     format!(
                "There is no connector named `{name}`. Valid group names are the ones listed in \
                 the MCP servers table of your context, plus the reserved `config`. Do not guess \
                 a name; if the user needs this capability, they can look for it in the Connectors \
                 marketplace."
            ),
        }
    }

    /// A diagnosis lookup: `Err` is a broken query, not an answer — log it and
    /// fall through to the next candidate rather than mislabelling the group.
    fn lookup<T>(&self, r: anyhow::Result<T>, table: &str, name: &str) -> Option<T> {
        match r {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::warn!(table, group = name, error = %e, "activate_tools: diagnosis lookup failed");
                None
            }
        }
    }

    /// The catalog blurb of the entry a user activation came from — the only
    /// description a non-running connector has.
    async fn catalog_description(&self, catalog_name: Option<&str>) -> Option<String> {
        let name = catalog_name?;
        mcp_catalog::get_by_name(&self.shared_pool, name).await.ok().flatten()?.description
    }

    /// How this connector's sign-in is completed, per its catalog `auth_kind`.
    async fn login_hint(&self, catalog_name: Option<&str>) -> &'static str {
        let kind = match catalog_name {
            Some(n) => mcp_catalog::get_by_name(&self.shared_pool, n)
                .await
                .ok()
                .flatten()
                .map(|c| c.auth_kind),
            None => None,
        };
        match kind.as_deref() {
            Some("oauth") => "complete the sign-in (approve access, then paste the code back)",
            Some("qr")    => "scan the QR code with the device they want to link",
            _             => "finish setting it up",
        }
    }
}

#[agent_loop::async_trait]
impl ToolActivator for SkaldToolActivator {
    async fn activate(&self, groups: Vec<String>, ctx: &ToolCtx) -> Result<String, ToolFailure> {
        if groups.is_empty() {
            return Err(ToolFailure::Failed("activate_tools: `groups` is empty".into()));
        }

        // Resolve first, act only on what resolved: an unknown or unconfigured
        // group must leave no trace, in RAM or in the DB.
        let mut reports: Vec<(String, GroupReport)> = Vec::new();
        for g in &groups {
            if reports.iter().any(|(n, _)| n == g) {
                continue; // the same group twice in one call
            }
            let report = self.resolve(g).await;
            reports.push((g.clone(), report));
        }

        let activated: Vec<String> = reports
            .iter()
            .filter(|(_, r)| r.status == Status::Activated)
            .map(|(n, _)| n.clone())
            .collect();

        if !activated.is_empty() {
            // Immediate in-memory effect (the defs re-read at the next round
            // picks the new grants up for free).
            {
                let mut set = self
                    .grants
                    .write()
                    .map_err(|_| ToolFailure::Failed("activate_tools: lock poisoned".into()))?;
                for g in &activated {
                    set.insert(g.clone());
                }
            }

            // Durable effect, anchored at the triggering assistant message. The
            // anchor is derived from the call row — the crate's ToolCtx carries
            // the call id, the message id is one lookup away.
            let call = chat_llm_tools::get(&self.pool, ctx.call_id.get())
                .await
                .map_err(|e| ToolFailure::Failed(format!("activate_tools: anchor lookup failed: {e}")))?
                .ok_or_else(|| ToolFailure::Failed("activate_tools: call row not found".into()))?;
            for g in &activated {
                let kind = if g == CONFIG_GROUP { "builtin" } else { "mcp" };
                activated_tools::grant(&self.pool, self.session_id, self.stack, call.message_id, kind, g)
                    .await
                    .map_err(|e| ToolFailure::Failed(format!("activate_tools: grant failed: {e}")))?;
            }
        }

        // One JSON object keyed by group name, same shape whether the call
        // succeeded or not — the model parses one thing, never prose.
        let body = Value::Object(
            reports
                .iter()
                .map(|(name, r)| (name.clone(), r.to_json()))
                .collect(),
        );
        let text = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());

        if activated.is_empty() {
            // Nothing was activated: fail, so the model treats it as an error
            // and relays the diagnosis instead of proceeding as if it worked.
            return Err(ToolFailure::Failed(text));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use agent_loop::store::HistoryStore;
    use agent_loop::tool::ToolOutput;
    use mcp_client::McpTool;

    use crate::db::{chat_history, chat_sessions_stack};
    use crate::loop_adapters::history::SqliteHistory;
    use crate::tools::ToolResult;

    struct FakeMcp {
        tools: Vec<McpTool>,
    }

    impl FakeMcp {
        fn with_server(name: &str, tool_names: &[&str]) -> Self {
            Self {
                tools: tool_names
                    .iter()
                    .map(|t| McpTool {
                        server_name:   name.to_string(),
                        name:          t.to_string(),
                        description:   String::new(),
                        input_schema:  serde_json::json!({"type":"object"}),
                        title:         None,
                        output_schema: None,
                        annotations:   None,
                        task_support:  None,
                    })
                    .collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl McpProvider for FakeMcp {
        fn tools(&self) -> Vec<McpTool> { self.tools.clone() }
        fn tools_for(&self, names: &[String]) -> Vec<McpTool> {
            self.tools.iter().filter(|t| names.contains(&t.server_name)).cloned().collect()
        }
        fn server_descriptions(&self) -> HashMap<String, Option<String>> { HashMap::new() }
        fn server_infos(&self) -> Vec<Value> { Vec::new() }
        fn tool_display_name(&self, _server: &str, _tool: &str) -> Option<String> { None }
        async fn call(&self, _server: &str, _tool: &str, _args: Value) -> anyhow::Result<ToolResult> {
            unimplemented!()
        }
    }


    fn temp_db_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        p.push(format!("skald-test-{tag}-{}-{nanos}.db", std::process::id()));
        p.to_string_lossy().into_owned()
    }

    fn cleanup(path: &str) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
    }

    struct Fixture {
        pool:  Arc<SqlitePool>,
        frame: FrameId,
        msg:   MessageId,
        call:  agent_loop::ids::ToolCallId,
        path:  String,
    }

    async fn fixture(tag: &str) -> Fixture {
        let path = temp_db_path(tag);
        let pool = Arc::new(crate::db::init_system_pool(&path).await.unwrap());
        sqlx::query("INSERT INTO chat_sessions (id) VALUES (1)").execute(&*pool).await.unwrap();
        let frame_row = chat_sessions_stack::create(&pool, 1, "assistant", None, 0, None).await.unwrap();
        let msg = chat_history::append(&pool, frame_row.id, &chat_history::Role::Assistant, "activating", false, None)
            .await
            .unwrap();
        let call = chat_llm_tools::append(&pool, msg, "activate_tools", "{}").await.unwrap();
        Fixture {
            pool,
            frame: FrameId(frame_row.id),
            msg: MessageId(msg),
            call: agent_loop::ids::ToolCallId(call),
            path,
        }
    }

    /// The fixture's pool is both the owner and the registry pool: `init_system_pool`
    /// creates both buckets in one file, which is exactly what a diagnosis needs.
    fn activator(f: &Fixture, mcp: Arc<dyn McpProvider>, grants: Arc<RwLock<HashSet<String>>>) -> SkaldToolActivator {
        SkaldToolActivator::new(
            f.pool.clone(),
            f.pool.clone(),
            "u1".into(),
            mcp,
            Arc::new(vec![serde_json::json!({"type":"function","function":{"name":"cron_list"}})]),
            grants,
            1,
            None,
        )
    }

    fn ctx_of(f: &Fixture) -> ToolCtx {
        ToolCtx {
            conversation: agent_loop::ids::ConversationId::new("session:1"),
            frame:        f.frame,
            agent:        "assistant".into(),
            call_id:      f.call,
            cancel:       tokio_util::sync::CancellationToken::new(),
            extensions:   Default::default(),
        }
    }

    fn report(text: &str, group: &str) -> Value {
        serde_json::from_str::<Value>(text).expect("tool result is JSON")[group].clone()
    }

    #[tokio::test]
    async fn activate_grants_in_memory_and_persists_anchored() {
        let f = fixture("act-grant").await;
        let mcp: Arc<dyn McpProvider> = Arc::new(FakeMcp::with_server("gmail", &["send", "read"]));
        let grants = Arc::new(RwLock::new(HashSet::new()));
        let activator = activator(&f, mcp, grants.clone());

        let text = activator
            .activate(vec!["gmail".into(), CONFIG_GROUP.into()], &ctx_of(&f))
            .await
            .unwrap();

        let gmail = report(&text, "gmail");
        assert_eq!(gmail["status"], "activated");
        assert_eq!(gmail["tool_prefix"], "mcp__gmail__");
        assert_eq!(gmail["tool_count"], 2);
        assert_eq!(report(&text, CONFIG_GROUP)["status"], "activated");
        assert_eq!(report(&text, CONFIG_GROUP)["tool_count"], 1);

        // In-memory effect.
        assert!(grants.read().unwrap().contains("gmail"));
        assert!(grants.read().unwrap().contains(CONFIG_GROUP));

        // Durable effect, anchored at the assistant message.
        let refs = activated_tools::list_refs_session(&f.pool, 1).await.unwrap();
        assert_eq!(refs.len(), 2);
        let acts = activated_tools::list_active_at(&f.pool, 1, None, i64::MAX).await.unwrap();
        assert!(acts.iter().all(|a| a.message_id == f.msg.get()));

        f.pool.close().await;
        cleanup(&f.path);
    }

    /// The bug this taxonomy exists for: a name nobody knows used to be granted,
    /// persisted and reported as a success.
    #[tokio::test]
    async fn unknown_group_fails_and_leaves_no_trace() {
        let f = fixture("act-unknown").await;
        let mcp: Arc<dyn McpProvider> = Arc::new(FakeMcp::with_server("tavily", &["search"]));
        let grants = Arc::new(RwLock::new(HashSet::new()));
        let activator = activator(&f, mcp, grants.clone());

        let err = activator.activate(vec!["gmail".into()], &ctx_of(&f)).await.unwrap_err();
        let ToolFailure::Failed(text) = err else { panic!("expected Failed") };
        assert_eq!(report(&text, "gmail")["status"], "unknown");

        assert!(grants.read().unwrap().is_empty(), "no in-memory grant");
        assert!(
            activated_tools::list_refs_session(&f.pool, 1).await.unwrap().is_empty(),
            "no durable row"
        );

        f.pool.close().await;
        cleanup(&f.path);
    }

    /// Activated by the user, sign-in never completed (§15): diagnosed, not granted.
    #[tokio::test]
    async fn pending_activation_reports_needs_login() {
        let f = fixture("act-pending").await;
        crate::db::mcp_catalog::upsert(&f.pool, catalog_entry("gmail", "per_user", "oauth")).await.unwrap();
        crate::db::mcp_user_servers::insert(&f.pool, mcp_user_servers::InsertUserServer {
            name: "gmail", catalog_name: Some("gmail"), source: "local_script", transport: "stdio",
            command: Some("node"), args_json: None, env_json: None, url: None, api_key: None,
            oauth_provider: Some("google"), deliver_json: None, script_rel_path: None,
            verify_command: None, verify_script_rel_path: None, auth_state: "pending",
        }).await.unwrap();

        let mcp: Arc<dyn McpProvider> = Arc::new(FakeMcp::with_server("tavily", &["search"]));
        let grants = Arc::new(RwLock::new(HashSet::new()));
        let activator = activator(&f, mcp, grants.clone());

        let err = activator.activate(vec!["gmail".into()], &ctx_of(&f)).await.unwrap_err();
        let ToolFailure::Failed(text) = err else { panic!("expected Failed") };
        let r = report(&text, "gmail");
        assert_eq!(r["status"], "needs_login");
        assert_eq!(r["description"], "Mail for the user");
        assert!(r["message"].as_str().unwrap().contains("paste the code"), "{r}");

        assert!(grants.read().unwrap().is_empty());
        assert!(activated_tools::list_refs_session(&f.pool, 1).await.unwrap().is_empty());

        f.pool.close().await;
        cleanup(&f.path);
    }

    /// In the catalog, never granted to this user: the admin is the one who can fix it.
    #[tokio::test]
    async fn catalog_entry_without_grant_reports_not_authorized() {
        let f = fixture("act-cat").await;
        crate::db::mcp_catalog::upsert(&f.pool, catalog_entry("gmail", "per_user", "oauth")).await.unwrap();

        let mcp: Arc<dyn McpProvider> = Arc::new(FakeMcp::with_server("tavily", &["search"]));
        let grants = Arc::new(RwLock::new(HashSet::new()));
        let activator = activator(&f, mcp, grants.clone());

        let err = activator.activate(vec!["gmail".into()], &ctx_of(&f)).await.unwrap_err();
        let ToolFailure::Failed(text) = err else { panic!("expected Failed") };
        assert_eq!(report(&text, "gmail")["status"], "not_authorized");

        f.pool.close().await;
        cleanup(&f.path);
    }

    /// A mixed batch activates what it can and diagnoses the rest — the whole
    /// call is not lost because one name was wrong.
    #[tokio::test]
    async fn mixed_batch_activates_only_the_resolvable_ones() {
        let f = fixture("act-mixed").await;
        let mcp: Arc<dyn McpProvider> = Arc::new(FakeMcp::with_server("tavily", &["search"]));
        let grants = Arc::new(RwLock::new(HashSet::new()));
        let activator = activator(&f, mcp, grants.clone());

        let text = activator
            .activate(vec!["tavily".into(), "gmail".into()], &ctx_of(&f))
            .await
            .unwrap();
        assert_eq!(report(&text, "tavily")["status"], "activated");
        assert_eq!(report(&text, "gmail")["status"], "unknown");

        let set = grants.read().unwrap().clone();
        assert_eq!(set, HashSet::from(["tavily".to_string()]));
        assert_eq!(activated_tools::list_refs_session(&f.pool, 1).await.unwrap(), vec!["tavily"]);

        f.pool.close().await;
        cleanup(&f.path);
    }

    fn catalog_entry<'a>(name: &'a str, scope: &'a str, auth_kind: &'a str) -> mcp_catalog::UpsertCatalog<'a> {
        mcp_catalog::UpsertCatalog {
            name, scope, source: "local_script", transport: "stdio",
            command: Some("node"), args_json: None, env_json: None, url: None,
            script_path: Some("gmail/server.js"), config_schema_json: None, auth_kind,
            oauth_provider: Some("google"), oauth_scopes_json: None, deliver_json: None,
            role_filter: None, verify_command: None, verify_script_path: None,
            icon_small_path: None, icon_large_path: None, friendly_name: Some("Gmail"),
            description: Some("Mail for the user"), tool_meta_json: None,
            version: None, version_string: None, version_release_date: None,
        }
    }

    #[tokio::test]
    async fn activation_source_resolves_defs_per_anchor() {
        let f = fixture("act-src").await;
        let mcp: Arc<dyn McpProvider> = Arc::new(FakeMcp::with_server("gmail", &["send", "read"]));
        activated_tools::grant(&f.pool, 1, None, f.msg.get(), "mcp", "gmail").await.unwrap();
        activated_tools::grant(&f.pool, 1, None, f.msg.get(), "builtin", CONFIG_GROUP).await.unwrap();

        let config_defs = Arc::new(vec![serde_json::json!({
            "type":"function","function":{"name":"cron_list","parameters":{"type":"object"}}
        })]);
        let src = SkaldActivationSource::new(f.pool.clone(), mcp, config_defs, 1, None);
        let acts = src.activations(f.frame).await.unwrap();

        assert_eq!(acts.len(), 1, "same anchor → one merged entry");
        let names: Vec<&str> = acts[0]
            .defs
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect();
        assert!(names.contains(&"send") || names.iter().any(|n| n.contains("send")), "{names:?}");
        assert!(names.contains(&"cron_list"), "{names:?}");

        // The SqliteHistory + LinearAssembler path agrees on the anchor type.
        let store = SqliteHistory::new(f.pool.clone());
        let history = store.load(f.frame).await.unwrap();
        assert_eq!(history[0].id, f.msg);
        let _ = ToolOutput::Text("unused".into());

        f.pool.close().await;
        cleanup(&f.path);
    }
}
