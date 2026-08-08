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

use std::sync::Arc;
use std::sync::atomic::Ordering;

use agent_loop::events::{EventSink, LoopEvent};
use agent_loop::gate::{Gate, GateDecision, PendingCall};
use agent_loop::store::{CallState, HistoryStore};
use core_api::user_fs::SharedFs;
use sqlx::SqlitePool;

use crate::approval::{ApprovalManager, GateResult};
use crate::loop_adapters::scope::TurnScope;
use crate::run_context::RunContext;
use crate::session::handler::ApprovalDecision;
use crate::tools::{ToolRegistry, is_file_read_tool, is_file_write_tool, tool_names as tn};

/// The gate's **long-lived** dependencies: it is built once per user, and reads
/// the turn's own state (session, source, group, run context) from the call's
/// [`TurnScope`] instead of capturing it.
pub struct ApprovalGate {
    approval:    Arc<ApprovalManager>,
    store:       Arc<dyn HistoryStore>,
    tools:       Arc<ToolRegistry>,
    /// For the `PendingWrite` diff: owner pool (user-memory), shared pool
    /// (shared-memory), and the caller's fs view (host paths).
    pool:        Arc<SqlitePool>,
    shared_pool: Arc<SqlitePool>,
    fs:          Option<SharedFs>,
}

impl ApprovalGate {
    pub fn new(
        approval:    Arc<ApprovalManager>,
        store:       Arc<dyn HistoryStore>,
        tools:       Arc<ToolRegistry>,
        pool:        Arc<SqlitePool>,
        shared_pool: Arc<SqlitePool>,
        fs:          Option<SharedFs>,
    ) -> Self {
        Self { approval, store, tools, pool, shared_pool, fs }
    }

    /// Reads the current content of a file for the `PendingWrite` diff, routed
    /// exactly like the fs-tools (memory notes → the right pool, everything
    /// else → the caller's host workspace, containment-checked).
    async fn read_current_content(&self, path: &str) -> Option<String> {
        use crate::tools::fs::{MemScope, classify_memory, resolve_host_path};
        if let Some(m) = classify_memory(path) {
            let pool = match m.scope {
                MemScope::User   => &self.pool,
                MemScope::Shared => &self.shared_pool,
            };
            return crate::db::memory_docs::get(pool, &m.rel)
                .await.ok().flatten().map(|d| d.content);
        }
        let fs = self.fs.as_ref()?;
        let abs = resolve_host_path(&fs.load(), path).ok()?;
        tokio::fs::read_to_string(&abs).await.ok()
    }

    /// Computes what a file would look like after the tool runs, without
    /// writing it. `None` if indeterminable (e.g. edit on a missing file).
    async fn compute_new_content(&self, name: &str, args: &serde_json::Value) -> Option<String> {
        match name {
            "write_file" => args["content"].as_str().map(|s| s.to_string()),
            "edit_file" => {
                let path     = args["path"].as_str()?;
                let old_text = args["old"].as_str()?;
                let new_text = args["new"].as_str()?;
                let current  = self.read_current_content(path).await?;
                if current.contains(old_text) {
                    Some(current.replacen(old_text, new_text, 1))
                } else {
                    None
                }
            }
            "insert_at_line" => {
                let path      = args["path"].as_str()?;
                let line_num  = args["line"].as_u64()? as usize;
                let new_text  = args["content"].as_str()?;
                let placement = args["placement"].as_str().unwrap_or("after");
                if line_num == 0 { return None; }
                let current = self.read_current_content(path).await?;
                let mut lines: Vec<&str> = current.split('\n').collect();
                let idx        = (line_num - 1).min(lines.len().saturating_sub(1));
                let insert_idx = if placement == "before" { idx } else { idx + 1 };
                let new_lines: Vec<&str> = new_text.split('\n').collect();
                for (i, l) in new_lines.iter().enumerate() {
                    lines.insert(insert_idx + i, l);
                }
                Some(lines.join("\n"))
            }
            "replace_lines" => {
                let path      = args["path"].as_str()?;
                let from_line = args["from_line"].as_u64()? as usize;
                let to_line   = args["to_line"].as_u64()? as usize;
                let new_text  = args["new"].as_str()?;
                if from_line == 0 || to_line < from_line { return None; }
                let current = self.read_current_content(path).await?;
                let mut lines: Vec<&str> = current.lines().collect();
                let total = lines.len();
                if from_line > total { return None; }
                let to_clamped = to_line.min(total);
                let new_lines: Vec<&str> = new_text.lines().collect();
                lines.splice((from_line - 1)..to_clamped, new_lines);
                let has_trailing = current.ends_with('\n');
                let mut result = lines.join("\n");
                if has_trailing { result.push('\n'); }
                Some(result)
            }
            _ => None,
        }
    }

    /// Builds the review card for a pending `skill_register`: the destination's
    /// agent path, the installed body if this replaces one, and the candidate's
    /// own `SKILL.md`.
    ///
    /// Resolved through the caller's own `UserFs`, like every other path the gate
    /// touches, so a source that lives only in the container (`/tmp/…`) yields
    /// `None` here and a spoken refusal from the tool.
    async fn skill_registration_preview(
        &self,
        args: &serde_json::Value,
    ) -> Option<(String, Option<String>, String)> {
        use crate::skills::{Scope, install};
        use crate::tools::fs::{FsTarget, resolve_target};

        let scope = Scope::parse(args["scope"].as_str()?).ok()?;
        let fs = self.fs.as_ref()?.load();
        let host = match resolve_target(&fs, args["path"].as_str()?).ok()? {
            FsTarget::Host(p)          => p,
            FsTarget::Container { .. } => return None,
        };
        install::preview(&fs, scope, &host)
    }

    /// Emits the approval event for the tool kind: `PendingWrite` (via
    /// `LoopEvent::Host`) for file-write tools and `execute_cmd`,
    /// `ApprovalRequired` otherwise (port of `emit_approval_event`).
    async fn emit_approval_event(
        &self,
        events:     &EventSink,
        call:       &PendingCall,
        request_id: i64,
    ) {
        let name = call.name.as_str();
        if is_file_write_tool(name) {
            let path = call.args["path"].as_str().unwrap_or("").to_string();
            let (old_content, new_content) = tokio::join!(
                self.read_current_content(&path),
                self.compute_new_content(name, &call.args),
            );
            if let Some(new_content) = new_content {
                events.emit(call.frame, call.parent_frame, LoopEvent::Host(serde_json::json!({
                    "type":         "pending_write",
                    "request_id":   request_id,
                    "tool_call_id": call.id.get(),
                    "path":         path,
                    "old_content":  old_content,
                    "new_content":  new_content,
                })));
                return;
            }
        } else if name == tn::SKILL_REGISTER {
            // The review moment of the whole design (blueprint §9.1): for the
            // group's scope this is the *only* time a person reads a text that
            // will enter everybody's prompt. So the card carries the candidate's
            // `SKILL.md` in full — not its name, not a summary — with a header
            // naming the scope, the file list and whether it replaces something;
            // on a replacement the installed body goes in as `old_content`, and
            // the existing diff renderer turns the card into a review of what
            // actually changes. Reusing `pending_write` is what makes that free:
            // no new event, no new frontend, exactly as `execute_cmd` below.
            if let Some(preview) = self.skill_registration_preview(&call.args).await {
                let (path, old_content, new_content) = preview;
                events.emit(call.frame, call.parent_frame, LoopEvent::Host(serde_json::json!({
                    "type":         "pending_write",
                    "request_id":   request_id,
                    "tool_call_id": call.id.get(),
                    "path":         path,
                    "old_content":  old_content,
                    "new_content":  new_content,
                })));
                return;
            }
            // Unreadable or invalid source: fall through to the plain card. The
            // tool refuses it a moment later with a message that says why, and a
            // half-built preview would only make the refusal look like a bug.
        } else if name == tn::EXECUTE_CMD {
            let cmd = call.args["command"].as_str().unwrap_or("");
            events.emit(call.frame, call.parent_frame, LoopEvent::Host(serde_json::json!({
                "type":         "pending_write",
                "request_id":   request_id,
                "tool_call_id": call.id.get(),
                "path":         "$ execute_cmd",
                "old_content":  serde_json::Value::Null,
                "new_content":  format!("$ {cmd}"),
            })));
            return;
        }
        events.emit(call.frame, call.parent_frame, LoopEvent::ApprovalRequired {
            id: call.id,
            name: call.name.clone(),
            args: call.args.clone(),
            request_id,
        });
    }
}

#[agent_loop::async_trait]
impl Gate for ApprovalGate {
    async fn check(&self, call: &PendingCall, events: &EventSink) -> GateDecision {
        // No scope = a wiring bug. Denying is the only safe reading: an
        // unscoped call cannot be evaluated against any policy.
        let Some(scope) = TurnScope::from(&call.extensions) else {
            return GateDecision::Reject {
                reason: "approval: the turn published no scope; refusing to run the tool"
                    .to_string(),
            };
        };

        // Post-restart manual resolve: already approved via a resolve endpoint.
        if scope.pre_approved.lock().unwrap().remove(&call.id.get()) {
            return GateDecision::Allow;
        }

        let category = self.tools.category_of(&call.name);

        // The approval engine decides first: an explicit Deny/Allow rule wins.
        let mut gate = self
            .approval
            .check(
                scope.session_id,
                category,
                &call.agent,
                &scope.source,
                &call.name,
                &call.args,
                scope.group_id.as_deref(),
            )
            .await;

        // RunContext fast-path: relax `Require` for pre-authorized fs paths
        // (never overrides a Deny).
        if matches!(gate, GateResult::Require) {
            let path = call.args["path"].as_str().unwrap_or("");
            let guard = scope.run_context.read().await.clone();
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
                if scope.auto_deny.load(Ordering::Relaxed) {
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

                let label = scope.context_label.read().ok().and_then(|g| g.clone());
                let (request_id, approve_rx) = self
                    .approval
                    .register(
                        scope.session_id,
                        call.id.get(),
                        &call.name,
                        call.args.clone(),
                        &call.agent,
                        &scope.source,
                        label.as_deref(),
                        category,
                    )
                    .await;
                self.emit_approval_event(events, call, request_id).await;

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
    use std::collections::HashSet;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex;
    use tokio::sync::RwLock;

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

    /// The scope a turn publishes, with the knobs a test wants to vary.
    fn scope(source: &str, auto_deny: bool) -> Arc<TurnScope> {
        Arc::new(TurnScope {
            session_id:     1,
            source:         source.to_string(),
            is_interactive: source == "web",
            agent_id:       "assistant".into(),
            scratchpad_sid: 1,
            project_root:   None,
            context_label:  Arc::new(std::sync::RwLock::new(None)),
            run_context:    Arc::new(RwLock::new(None)),
            group_id:       None,
            pre_approved:   Arc::new(Mutex::new(HashSet::new())),
            auto_deny:      Arc::new(AtomicBool::new(auto_deny)),
            grants:         Arc::new(std::sync::RwLock::new(HashSet::new())),
            base_defs:      Arc::new(Vec::new()),
            config_defs:    Arc::new(Vec::new()),
            memory_tools:   Arc::new(Vec::new()),
            image_tools:    Arc::new(Vec::new()),
            root_only:      Arc::new(Vec::new()),
        })
    }

    /// A `PendingCall` carrying its turn's scope, as the kernel builds it.
    fn pending(call_id: i64, frame: i64, scope: Arc<TurnScope>) -> PendingCall {
        let mut extensions = Extensions::new();
        extensions.insert(scope);
        PendingCall {
            id: ToolCallId(call_id),
            name: "some_tool".into(),
            args: json!({}),
            frame: FrameId(frame),
            parent_frame: None,
            agent: "assistant".into(),
            extensions,
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
            pool.clone(),
            pool.clone(),
            None,
        );
        let (bus, _) = tokio::sync::broadcast::channel(16);
        let events = EventSink::new(ConversationId::new("session:1"), bus);
        let call = pending(call_id, frame.id, scope("web", false));
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
            pool.clone(),
            pool.clone(),
            None,
        );
        let (bus, _) = tokio::sync::broadcast::channel(16);
        let events = EventSink::new(ConversationId::new("session:1"), bus);
        // A background source that cannot ask a human.
        let call = pending(call_id, frame.id, scope("cron", true));

        // No rules at all → the seeded-less default is Require; auto-deny rejects.
        let d = gate.check(&call, &events).await;
        assert!(matches!(d, GateDecision::Reject { .. }));

        pool.close().await;
        cleanup(&path);
    }

    /// A call with no scope means the turn was wired wrong. Denying is the only
    /// safe reading — there is no policy to evaluate it against.
    #[tokio::test]
    async fn an_unscoped_call_is_denied() {
        let f = fixture("gate-unscoped").await;
        let mut call = f.call.clone();
        call.extensions = Extensions::new();

        let d = f.gate.check(&call, &f.events).await;
        match d {
            GateDecision::Reject { reason } => assert!(reason.contains("no scope"), "{reason}"),
            other => panic!("expected Reject, got {other:?}"),
        }

        f.pool.close().await;
        cleanup(&f.path);
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
