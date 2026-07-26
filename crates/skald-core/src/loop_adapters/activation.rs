//! DTL activation adapters (blueprint D15): the crate owns the wire protocol,
//! Skald owns the catalog (MCP servers + the reserved `config` group) and the
//! persistence (`activated_tools`, anchored at the triggering message).

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use agent_loop::activation::{Activation, ActivationSource, ToolActivator};
use agent_loop::ids::{FrameId, MessageId};
use agent_loop::tool::{ToolCtx, ToolFailure};
use serde_json::Value;
use sqlx::SqlitePool;

use crate::db::{activated_tools, chat_llm_tools};
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

/// Backend of the crate's shipped `activate_tools` tool: validates the groups
/// against the catalog, updates the in-memory grant set **immediately** (next
/// round sees the tools), and persists the activation anchored at the
/// triggering assistant message (derived from the call's `chat_llm_tools`
/// row). Unifies what today lives split between `tools/activate_tools.rs`
/// (grants) and `llm_loop.rs` (persistence).
pub struct SkaldToolActivator {
    pool:        Arc<SqlitePool>,
    mcp:         Arc<dyn McpProvider>,
    grants:      Arc<RwLock<HashSet<String>>>,
    session_id:  i64,
    stack:       Option<i64>,
}

impl SkaldToolActivator {
    pub fn new(
        pool:       Arc<SqlitePool>,
        mcp:        Arc<dyn McpProvider>,
        grants:     Arc<RwLock<HashSet<String>>>,
        session_id: i64,
        stack:      Option<i64>,
    ) -> Self {
        Self { pool, mcp, grants, session_id, stack }
    }
}

#[agent_loop::async_trait]
impl ToolActivator for SkaldToolActivator {
    async fn activate(&self, groups: Vec<String>, ctx: &ToolCtx) -> Result<String, ToolFailure> {
        if groups.is_empty() {
            return Err(ToolFailure::Failed("activate_tools: `groups` is empty".into()));
        }

        let available: HashSet<String> = self.mcp.tools().iter().map(|t| t.server_name.clone()).collect();

        // Immediate in-memory effect (the defs re-read at the next round picks
        // the new grants up for free).
        {
            let mut set = self.grants.write().map_err(|_| ToolFailure::Failed("activate_tools: lock poisoned".into()))?;
            for g in &groups {
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
        for g in &groups {
            let kind = if g == CONFIG_GROUP { "builtin" } else { "mcp" };
            activated_tools::grant(&self.pool, self.session_id, self.stack, call.message_id, kind, g)
                .await
                .map_err(|e| ToolFailure::Failed(format!("activate_tools: grant failed: {e}")))?;
        }

        let activated: Vec<String> = groups
            .iter()
            .map(|n| {
                if n == CONFIG_GROUP || available.contains(n) {
                    format!("{n} ✓")
                } else {
                    format!("{n} (registered but not yet running — tools will appear after reconnect)")
                }
            })
            .collect();
        let scope = match self.stack {
            None    => "session".to_string(),
            Some(s) => format!("stack {s}"),
        };
        Ok(format!(
            "Tool groups activated for this {scope}: {}. \
             Their tools are available from the next tool-call round.",
            activated.join(", ")
        ))
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

    #[tokio::test]
    async fn activate_grants_in_memory_and_persists_anchored() {
        let f = fixture("act-grant").await;
        let mcp: Arc<dyn McpProvider> = Arc::new(FakeMcp::with_server("gmail", &["send", "read"]));
        let grants = Arc::new(RwLock::new(HashSet::new()));
        let activator = SkaldToolActivator::new(f.pool.clone(), mcp, grants.clone(), 1, None);

        let ctx = ToolCtx {
            conversation: agent_loop::ids::ConversationId::new("session:1"),
            frame:        f.frame,
            agent:        "assistant".into(),
            call_id:      f.call,
            cancel:       tokio_util::sync::CancellationToken::new(),
            extensions:   Default::default(),
        };
        let text = activator.activate(vec!["gmail".into(), CONFIG_GROUP.into()], &ctx).await.unwrap();
        assert!(text.contains("gmail ✓"));

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
