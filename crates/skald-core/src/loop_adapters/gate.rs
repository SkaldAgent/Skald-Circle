//! `ApprovalGate` — Skald's approval flow behind the crate's `Gate` trait
//! (port of `handler/gate.rs::run_approval_gate`, blueprint §10):
//!
//! 1. `pre_approved` short-circuit (post-restart manual resolve);
//! 2. the approval engine decides (explicit Allow/Deny rules win);
//! 3. the RunContext fast-path relaxes `Require` to `Allow` for pre-authorized
//!    fs paths (never overrides a Deny);
//! 4. `Require` → auto-deny, or mark `AwaitingHuman` + register + emit
//!    `ApprovalRequired` + block on the human decision; a closed channel maps
//!    to `GateDecision::Suspend` (the call stays `AwaitingHuman`, the turn
//!    ends) — the old `GateOutcome::ChannelClosed`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use agent_loop::events::{EventSink, LoopEvent};
use agent_loop::gate::{Gate, GateDecision, PendingCall};
use agent_loop::store::{CallState, HistoryStore};

use crate::approval::{ApprovalManager, GateResult};
use crate::run_context::RunContext;
use crate::session::handler::ApprovalDecision;
use crate::tools::{ToolRegistry, is_file_read_tool, is_file_write_tool};

/// Everything the gate needs that the current loop keeps on the handler.
/// Shared by reference so phase-2 wiring shares the same cells.
pub struct ApprovalGate {
    approval:      Arc<ApprovalManager>,
    store:         Arc<dyn HistoryStore>,
    tools:         Arc<ToolRegistry>,
    session_id:    i64,
    source:        String,
    group_id:      Option<String>,
    run_context:   Arc<RwLock<Option<RunContext>>>,
    pre_approved:  Arc<Mutex<HashSet<i64>>>,
    auto_deny:     Arc<AtomicBool>,
    context_label: Arc<RwLock<Option<String>>>,
}

impl ApprovalGate {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        approval:      Arc<ApprovalManager>,
        store:         Arc<dyn HistoryStore>,
        tools:         Arc<ToolRegistry>,
        session_id:    i64,
        source:        impl Into<String>,
        group_id:      Option<String>,
        run_context:   Arc<RwLock<Option<RunContext>>>,
        pre_approved:  Arc<Mutex<HashSet<i64>>>,
        auto_deny:     Arc<AtomicBool>,
        context_label: Arc<RwLock<Option<String>>>,
    ) -> Self {
        Self {
            approval,
            store,
            tools,
            session_id,
            source: source.into(),
            group_id,
            run_context,
            pre_approved,
            auto_deny,
            context_label,
        }
    }
}

#[agent_loop::async_trait]
impl Gate for ApprovalGate {
    async fn check(&self, call: &PendingCall, events: &EventSink) -> GateDecision {
        // Post-restart manual resolve: already approved via a resolve endpoint.
        if self.pre_approved.lock().unwrap().remove(&call.id.get()) {
            return GateDecision::Allow;
        }

        let category = self.tools.category_of(&call.name);

        // The approval engine decides first: an explicit Deny/Allow rule wins.
        let mut gate = self
            .approval
            .check(
                self.session_id,
                category,
                &call.agent,
                &self.source,
                &call.name,
                &call.args,
                self.group_id.as_deref(),
            )
            .await;

        // RunContext fast-path: relax `Require` for pre-authorized fs paths
        // (never overrides a Deny).
        if matches!(gate, GateResult::Require) {
            let path = call.args["path"].as_str().unwrap_or("");
            let guard = self.run_context.read().map(|g| g.clone()).unwrap_or_default();
            let dflt = RunContext::default();
            let rc = guard.as_ref().unwrap_or(&dflt);
            let pre_allowed = if is_file_read_tool(&call.name) {
                rc.is_read_allowed(path)
            } else if is_file_write_tool(&call.name) {
                rc.is_write_allowed(path)
            } else {
                false
            };
            if pre_allowed {
                gate = GateResult::Allow;
            }
        }

        match gate {
            GateResult::Allow => GateDecision::Allow,
            GateResult::Deny => GateDecision::Reject {
                reason: "Tool call denied by approval policy.".to_string(),
            },
            GateResult::Require => {
                if self.auto_deny.load(Ordering::Relaxed) {
                    return GateDecision::Reject {
                        reason: "Tool call auto-denied: this session does not support approval requests."
                            .to_string(),
                    };
                }

                // Durability FIRST: the call must survive a crash as pending.
                if let Err(e) = self.store.set_call_state(call.id, CallState::AwaitingHuman).await {
                    return GateDecision::Reject {
                        reason: format!("approval: failed to mark call pending: {e}"),
                    };
                }

                let label = self.context_label.read().ok().and_then(|g| g.clone());
                let (request_id, approve_rx) = self
                    .approval
                    .register(
                        self.session_id,
                        call.id.get(),
                        &call.name,
                        call.args.clone(),
                        &call.agent,
                        &self.source,
                        label.as_deref(),
                        category,
                    )
                    .await;
                events.emit(call.frame, None, LoopEvent::ApprovalRequired {
                    id: call.id,
                    name: call.name.clone(),
                    args: call.args.clone(),
                });
                let _ = request_id;

                match approve_rx.await {
                    Ok(ApprovalDecision::Approved) => GateDecision::Allow,
                    Ok(ApprovalDecision::Rejected { note }) => GateDecision::Reject {
                        reason: ApprovalDecision::rejection_message(&note),
                    },
                    Err(_) => GateDecision::Suspend,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_loop::events::EventSink;
    use agent_loop::ids::{ConversationId, FrameId, ToolCallId};
    use agent_loop::tool::Extensions;
    use serde_json::json;
    use sqlx::SqlitePool;

    use crate::approval::{NewApprovalRule, RuleAction};
    use crate::db::{chat_history, chat_llm_tools, chat_sessions_stack};
    use crate::loop_adapters::history::SqliteHistory;

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
        gate:    ApprovalGate,
        events:  EventSink,
        pool:    Arc<SqlitePool>,
        call:    PendingCall,
        path:    String,
        approval: Arc<ApprovalManager>,
    }

    async fn fixture(tag: &str) -> Fixture {
        let path = temp_db_path(tag);
        let pool = Arc::new(crate::db::init_system_pool(&path).await.unwrap());
        // The `default` permission group is a FK target for approval_rules.group_id.
        sqlx::query("INSERT INTO tool_permission_groups (id, name) VALUES ('default', 'Default')")
            .execute(&*pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO chat_sessions (id) VALUES (1)").execute(&*pool).await.unwrap();
        let frame = chat_sessions_stack::create(&pool, 1, "assistant", None, 0, None).await.unwrap();
        let msg = chat_history::append(&pool, frame.id, &chat_history::Role::Assistant, "a", false, None)
            .await
            .unwrap();
        let call_id = chat_llm_tools::append(&pool, msg, "some_tool", "{}").await.unwrap();

        let (tx, _) = tokio::sync::broadcast::channel(16);
        let approval = Arc::new(ApprovalManager::new(pool.clone(), tx));
        let store: Arc<dyn HistoryStore> = Arc::new(SqliteHistory::new(pool.clone()));
        let tools = Arc::new(ToolRegistry::new());
        let gate = ApprovalGate::new(
            approval.clone(),
            store,
            tools,
            1,
            "web",
            None,
            Arc::new(RwLock::new(None)),
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(AtomicBool::new(false)),
            Arc::new(RwLock::new(None)),
        );
        let (bus, _) = tokio::sync::broadcast::channel(16);
        let events = EventSink::new(ConversationId::new("session:1"), bus);
        let call = PendingCall {
            id: ToolCallId(call_id),
            name: "some_tool".into(),
            args: json!({}),
            frame: FrameId(frame.id),
            agent: "assistant".into(),
            extensions: Extensions::new(),
        };
        Fixture { gate, events, pool, call, path, approval }
    }

    #[tokio::test]
    async fn explicit_deny_rule_rejects() {
        let f = fixture("gate-deny").await;
        f.approval
            .add_rule(NewApprovalRule {
                agent_id: None,
                source: None,
                tool_pattern: "some_tool".into(),
                path_pattern: None,
                action: RuleAction::Deny,
                note: None,
                priority: Some(1),
                group_id: None,
            })
            .await
            .unwrap();

        let d = f.gate.check(&f.call, &f.events).await;
        assert!(matches!(d, GateDecision::Reject { .. }));

        f.pool.close().await;
        cleanup(&f.path);
    }

    #[tokio::test]
    async fn auto_deny_rejects_require() {
        let path = temp_db_path("gate-autodeny");
        let pool = Arc::new(crate::db::init_system_pool(&path).await.unwrap());
        sqlx::query("INSERT INTO chat_sessions (id) VALUES (1)").execute(&*pool).await.unwrap();
        let frame = chat_sessions_stack::create(&pool, 1, "assistant", None, 0, None).await.unwrap();
        let msg = chat_history::append(&pool, frame.id, &chat_history::Role::Assistant, "a", false, None)
            .await
            .unwrap();
        let call_id = chat_llm_tools::append(&pool, msg, "some_tool", "{}").await.unwrap();

        let (tx, _) = tokio::sync::broadcast::channel(16);
        let approval = Arc::new(ApprovalManager::new(pool.clone(), tx));
        let gate = ApprovalGate::new(
            approval,
            Arc::new(SqliteHistory::new(pool.clone())),
            Arc::new(ToolRegistry::new()),
            1,
            "cron",                       // background source: auto-deny
            None,
            Arc::new(RwLock::new(None)),
            Arc::new(Mutex::new(HashSet::new())),
            Arc::new(AtomicBool::new(true)),
            Arc::new(RwLock::new(None)),
        );
        let (bus, _) = tokio::sync::broadcast::channel(16);
        let events = EventSink::new(ConversationId::new("session:1"), bus);
        let call = PendingCall {
            id: ToolCallId(call_id),
            name: "some_tool".into(),
            args: json!({}),
            frame: FrameId(frame.id),
            agent: "assistant".into(),
            extensions: Extensions::new(),
        };

        // No rules at all → the seeded-less default is Require; auto-deny rejects.
        let d = gate.check(&call, &events).await;
        assert!(matches!(d, GateDecision::Reject { .. }));

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn human_approval_allows_and_marks_pending_first() {
        let f = fixture("gate-human").await;
        let approval = f.approval.clone();
        let gate = Arc::new(f.gate);
        let events = f.events.clone();
        let call = f.call.clone();

        let check = tokio::spawn(async move { gate.check(&call, &events).await });

        // Wait for the request to register, then approve it.
        let request_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let pending = approval.list_pending().await;
                if let Some(p) = pending.first() {
                    break p.request_id;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        // The call is durably pending while the human decides.
        let row = chat_llm_tools::get(&f.pool, f.call.id.get()).await.unwrap().unwrap();
        assert_eq!(row.status, "pending");

        approval.resolve(request_id, ApprovalDecision::Approved).await;
        let d = check.await.unwrap();
        assert!(matches!(d, GateDecision::Allow));

        f.pool.close().await;
        cleanup(&f.path);
    }

    #[tokio::test]
    async fn human_rejection_rejects_with_note() {
        let f = fixture("gate-reject").await;
        let approval = f.approval.clone();
        let gate = Arc::new(f.gate);
        let events = f.events.clone();
        let call = f.call.clone();

        let check = tokio::spawn(async move { gate.check(&call, &events).await });

        let request_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let pending = approval.list_pending().await;
                if let Some(p) = pending.first() {
                    break p.request_id;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        approval
            .resolve(request_id, ApprovalDecision::Rejected { note: "too risky".into() })
            .await;
        let d = check.await.unwrap();
        match d {
            GateDecision::Reject { reason } => assert!(reason.contains("too risky")),
            other => panic!("expected Reject, got {other:?}"),
        }

        f.pool.close().await;
        cleanup(&f.path);
    }
}
