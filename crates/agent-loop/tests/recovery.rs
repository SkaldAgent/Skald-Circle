//! Recovery suite (blueprint §8/§13): the post-crash store is built **by hand**
//! on `InMemoryStore` — a call left `Running`, a child frame nobody closed, two
//! siblings of an interrupted batch — and recovery is asked to make it
//! well-formed again and continue.
//!
//! No DB, no network: the states a real crash produces are exactly the states a
//! test can write, because every transition is a store write.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_loop::context::StaticSystemContext;
use agent_loop::delegate::{AgentCatalog, AgentKind, AgentProfile, AgentSummary, ToolSelection};
use agent_loop::ids::{ConversationId, FrameId, ToolCallId};
use agent_loop::manager::{LoopManager, TurnMeta, TurnParams};
use agent_loop::model::{ModelHint, StaticModels};
use agent_loop::prelude::async_trait;
use agent_loop::recovery::{HumanDecision, PendingPolicy, RecoveryPolicy, RunningPolicy};
use agent_loop::store::{
    CallState, FrameSpec, HistoryStore, NewCall, NewMessage, StoredCall,
};
use agent_loop::store_memory::InMemoryStore;
use agent_loop::testing::{self, FakeModel, Step};
use agent_loop::tool::{
    RestartHint, Tool, ToolCtx, ToolFailure, ToolOutput, ToolRegistry, ToolSet,
};
use serde_json::{Value, json};

// ── tools ────────────────────────────────────────────────────────────────────

/// Idempotent: safe to re-run after a crash. Counts its executions.
struct Counter {
    runs: Arc<Mutex<usize>>,
}

#[async_trait]
impl Tool for Counter {
    fn name(&self) -> &str { "counter" }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{"name":"counter","parameters":{"type":"object"}}})
    }
    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        let mut runs = self.runs.lock().unwrap();
        *runs += 1;
        Ok(ToolOutput::Text(format!("run {runs}")))
    }
}

/// Non-idempotent (a shell command already had its effect): must NOT be re-run.
struct SideEffect {
    runs: Arc<Mutex<usize>>,
}

#[async_trait]
impl Tool for SideEffect {
    fn name(&self) -> &str { "shell" }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{"name":"shell","parameters":{"type":"object"}}})
    }
    fn restart_hint(&self) -> RestartHint { RestartHint::MarkInterrupted }
    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        *self.runs.lock().unwrap() += 1;
        Ok(ToolOutput::Text("ran".into()))
    }
}

/// Stands in for the delegate: recovery never calls it (a spawned frame is the
/// cascade's business), so running it at all is a bug.
struct NeverCalled;

#[async_trait]
impl Tool for NeverCalled {
    fn name(&self) -> &str { "delegate" }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{"name":"delegate","parameters":{"type":"object"}}})
    }
    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        panic!("recovery re-ran a sub-agent dispatch instead of cascading its frame");
    }
}

// ── catalog ──────────────────────────────────────────────────────────────────

/// Every child agent runs on the `child` model with its own prompt — so a test
/// can prove a resumed sub-agent came back as ITSELF (B3), not as the root.
struct Catalog;

#[async_trait]
impl AgentCatalog for Catalog {
    async fn get(
        &self,
        id:           &str,
        _child_frame: FrameId,
        _ctx:         &ToolCtx,
    ) -> agent_loop::Result<AgentProfile> {
        Ok(AgentProfile {
            id:        id.into(),
            kind:      AgentKind::Task,
            context:   Arc::new(StaticSystemContext::new(format!("You are {id}."))),
            tools:     ToolSelection::inherit(),
            toolset:   None,
            model:     Some(ModelHint::name("child")),
            selector:  None,
            assembler: None,
        })
    }
    async fn list(&self, _kind: AgentKind) -> Vec<AgentSummary> { Vec::new() }
}

// ── harness ──────────────────────────────────────────────────────────────────

struct H {
    manager: Arc<LoopManager>,
    store:   Arc<InMemoryStore>,
    tools:   Arc<dyn ToolSet>,
    conv:    ConversationId,
    root:    FrameId,
    counter: Arc<Mutex<usize>>,
    shell:   Arc<Mutex<usize>>,
    /// The child's script — a test asserting "the model was NOT called" leaves
    /// it empty, and `FakeModel` panics if anything pops from it.
    child:   Arc<FakeModel>,
}

impl H {
    async fn new(root_script: Vec<Step>, child_script: Vec<Step>) -> Self {
        let store = Arc::new(InMemoryStore::new());
        let root_model = Arc::new(FakeModel::new("root", root_script));
        let child = Arc::new(FakeModel::new("child", child_script));
        let manager = Arc::new(
            LoopManager::builder()
                .models(Arc::new(StaticModels::new(vec![
                    testing::handle(&root_model, "root"),
                    testing::handle(&child, "child"),
                ])))
                .store(store.clone())
                .build()
                .unwrap(),
        );
        let counter = Arc::new(Mutex::new(0));
        let shell = Arc::new(Mutex::new(0));
        let tools: Arc<dyn ToolSet> = ToolRegistry::new()
            .with(Counter { runs: counter.clone() })
            .with(SideEffect { runs: shell.clone() })
            .with(NeverCalled)
            .into_toolset();

        let conv = ConversationId::new("rec");
        let root = store
            .open_frame(&conv, None, FrameSpec::root("assistant"))
            .await
            .unwrap();
        Self { manager, store, tools, conv, root, counter, shell, child }
    }

    fn params(&self) -> TurnParams {
        TurnParams {
            frame:      self.root,
            agent:      "assistant".into(),
            system:     Arc::new(StaticSystemContext::new("You are the assistant.")),
            tools:      self.tools.clone(),
            model_hint: ModelHint::default(),
            selector:   None,
            live_input: None,
            extensions: Default::default(),
            meta:       TurnMeta::default(),
            assembler:  None,
        }
    }

    /// An assistant message with one call left in flight — what a crash leaves.
    async fn interrupted_call(&self, frame: FrameId, name: &str) -> ToolCallId {
        self.store.append(frame, NewMessage::user("do it")).await.unwrap();
        let msg = self
            .store
            .append(frame, NewMessage::assistant("calling", None))
            .await
            .unwrap();
        self.store.append_call(msg, NewCall::new(name, json!({}))).await.unwrap()
    }

    /// A child frame spawned by `call`, with its prompt already appended.
    async fn child_frame(&self, agent: &str, call: ToolCallId) -> FrameId {
        let frame = self
            .store
            .open_frame(&self.conv, Some(self.root), FrameSpec {
                agent:       agent.into(),
                prompt:      Some("go find out".into()),
                depth:       1,
                parent_call: Some(call),
                meta:        Value::Null,
            })
            .await
            .unwrap();
        self.store.append(frame, NewMessage::agent("go find out")).await.unwrap();
        frame
    }

    async fn call(&self, id: ToolCallId) -> StoredCall {
        self.store.get_call(id).await.unwrap().unwrap()
    }

    async fn recover_with(&self, policy: RecoveryPolicy) -> agent_loop::recovery::RecoveryReport {
        let recovery = self.manager.recovery(Arc::new(Catalog), policy);
        tokio::time::timeout(Duration::from_secs(5), recovery.run(&self.conv, &self.params()))
            .await
            .expect("recovery hung")
            .unwrap()
    }

    async fn recover(&self) -> agent_loop::recovery::RecoveryReport {
        self.recover_with(RecoveryPolicy::default()).await
    }
}

// ── interrupted calls ────────────────────────────────────────────────────────

#[tokio::test]
async fn an_interrupted_idempotent_call_is_re_executed_then_the_turn_continues() {
    let h = H::new(vec![Step::message("all done")], vec![]).await;
    let call = h.interrupted_call(h.root, "counter").await;

    let report = h.recover().await;

    assert_eq!(*h.counter.lock().unwrap(), 1, "the call must run exactly once");
    let call = h.call(call).await;
    assert_eq!(call.state, CallState::Done);
    assert_eq!(call.result.as_deref(), Some("run 1"));
    assert_eq!(report.calls_reexecuted, 1);
    assert_eq!(report.frames_resumed, 1, "the frame then ran a normal round");
}

#[tokio::test]
async fn an_interrupted_call_with_side_effects_is_failed_not_re_run() {
    // D7: `shell` declares MarkInterrupted, so re-running it could repeat an
    // effect that already happened.
    let h = H::new(vec![Step::message("I stopped mid-command")], vec![]).await;
    let call = h.interrupted_call(h.root, "shell").await;

    let report = h.recover().await;

    assert_eq!(*h.shell.lock().unwrap(), 0, "a non-idempotent tool must NOT be re-run");
    let call = h.call(call).await;
    assert_eq!(call.state, CallState::Failed);
    assert!(call.result.as_deref().unwrap().contains("interrupted"), "{:?}", call.result);
    assert_eq!(report.calls_failed, 1);
    assert_eq!(report.calls_reexecuted, 0);
}

#[tokio::test]
async fn the_policy_can_refuse_to_re_run_anything() {
    let h = H::new(vec![Step::message("continuing")], vec![]).await;
    let call = h.interrupted_call(h.root, "counter").await;

    h.recover_with(RecoveryPolicy {
        on_running: RunningPolicy::MarkInterrupted,
        ..RecoveryPolicy::default()
    })
    .await;

    assert_eq!(*h.counter.lock().unwrap(), 0, "the policy overrides the tool's hint");
    assert_eq!(h.call(call).await.state, CallState::Failed);
}

// ── awaiting human ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_call_awaiting_a_human_is_asked_again() {
    let h = H::new(vec![Step::message("approved and done")], vec![]).await;
    let call = h.interrupted_call(h.root, "counter").await;
    h.store.set_call_state(call, CallState::AwaitingHuman).await.unwrap();

    let report = h.recover().await;

    // ReAsk re-runs it through the gate — here an allowing one, so it executes.
    assert_eq!(*h.counter.lock().unwrap(), 1);
    assert_eq!(h.call(call).await.state, CallState::Done);
    assert_eq!(report.calls_reexecuted, 1);
    assert!(!report.left_pending);
}

#[tokio::test]
async fn leave_pending_stops_and_touches_nothing() {
    // No model step scripted: running the loop would panic, which is the point —
    // a frame with an unanswered call must not be driven.
    let h = H::new(vec![], vec![]).await;
    let call = h.interrupted_call(h.root, "counter").await;
    h.store.set_call_state(call, CallState::AwaitingHuman).await.unwrap();

    let report = h.recover_with(RecoveryPolicy {
        on_awaiting_human: PendingPolicy::LeavePending,
        ..RecoveryPolicy::default()
    })
    .await;

    assert!(report.left_pending);
    assert_eq!(report.frames_resumed, 0);
    assert_eq!(*h.counter.lock().unwrap(), 0);
    assert_eq!(h.call(call).await.state, CallState::AwaitingHuman, "still the human's to answer");
}

// ── the cascade ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_interrupted_sub_agent_finishes_as_itself_then_the_parent_continues() {
    let h = H::new(
        vec![Step::message("the root's final answer")],
        vec![Step::message("the child's answer")],
    )
    .await;
    let call = h.interrupted_call(h.root, "delegate").await;
    let child = h.child_frame("researcher", call).await;

    let report = h.recover().await;

    // The child ran under ITS agent's prompt and model (B3), not the root's.
    let seen = h.child.requests();
    assert_eq!(seen.len(), 1, "the child model ran exactly once");
    assert!(
        serde_json::to_string(&seen[0].messages).unwrap().contains("You are researcher."),
        "the resumed frame must run its own agent's context: {:?}",
        seen[0].messages
    );

    // Its answer became the parent call's result, and the child frame is closed.
    let call = h.call(call).await;
    assert_eq!(call.state, CallState::Done);
    assert_eq!(call.result.as_deref(), Some("the child's answer"));
    assert!(!h.store.get_frame(child).await.unwrap().unwrap().active);
    assert_eq!(report.frames_resumed, 2, "child then root");
}

#[tokio::test]
async fn a_child_that_finished_but_never_propagated_is_not_re_run() {
    // The wedge: the turn died in the instant between the child's last message
    // and its result reaching the parent. Re-running the model would ask it to
    // answer a question it already answered — the empty child script asserts
    // that never happens.
    let h = H::new(vec![Step::message("root wraps up")], vec![]).await;
    let call = h.interrupted_call(h.root, "delegate").await;
    let child = h.child_frame("researcher", call).await;
    h.store
        .append(child, NewMessage::assistant("already done", None))
        .await
        .unwrap();

    let report = h.recover().await;

    let call = h.call(call).await;
    assert_eq!(call.state, CallState::Done);
    assert_eq!(call.result.as_deref(), Some("already done"));
    assert_eq!(h.child.requests().len(), 0, "the child's LLM must not be called again");
    assert_eq!(report.frames_resumed, 1, "only the parent ran");
}

#[tokio::test]
async fn an_interrupted_parallel_batch_is_reaped_and_the_parent_resumes() {
    let h = H::new(vec![Step::message("carrying on without them")], vec![]).await;

    // Two delegate calls in one round, two live children: impossible for a
    // linear stack, so it can only be a batch caught mid-flight.
    h.store.append(h.root, NewMessage::user("do both")).await.unwrap();
    let msg = h.store.append(h.root, NewMessage::assistant("", None)).await.unwrap();
    let c1 = h.store.append_call(msg, NewCall::new("delegate", json!({}))).await.unwrap();
    let c2 = h.store.append_call(msg, NewCall::new("delegate", json!({}))).await.unwrap();
    let f1 = h.child_frame("a1", c1).await;
    let f2 = h.child_frame("a2", c2).await;

    let report = h.recover().await;

    assert_eq!(report.batches_reaped, 1);
    for (call, frame) in [(c1, f1), (c2, f2)] {
        let call = h.call(call).await;
        assert_eq!(call.state, CallState::Failed);
        assert!(call.result.as_deref().unwrap().contains("parallel batch"), "{:?}", call.result);
        assert!(!h.store.get_frame(frame).await.unwrap().unwrap().active);
    }
    assert_eq!(h.child.requests().len(), 0, "a reaped batch is not re-run");
    assert_eq!(report.frames_resumed, 1, "the root continues with the failures in view");
}

// ── resolve_pending ──────────────────────────────────────────────────────────

#[tokio::test]
async fn approving_after_a_restart_runs_the_call_and_continues() {
    let h = H::new(vec![Step::message("done, as approved")], vec![]).await;
    let call = h.interrupted_call(h.root, "counter").await;
    h.store.set_call_state(call, CallState::AwaitingHuman).await.unwrap();

    h.manager
        .resolve_pending(call, HumanDecision::Approved, Arc::new(Catalog), &h.params())
        .await
        .unwrap();

    assert_eq!(*h.counter.lock().unwrap(), 1);
    let call = h.call(call).await;
    assert_eq!(call.state, CallState::Done);
    assert_eq!(call.result.as_deref(), Some("run 1"));
}

#[tokio::test]
async fn rejecting_after_a_restart_records_the_refusal_and_continues() {
    let h = H::new(vec![Step::message("understood, I won't")], vec![]).await;
    let call = h.interrupted_call(h.root, "shell").await;
    h.store.set_call_state(call, CallState::AwaitingHuman).await.unwrap();

    h.manager
        .resolve_pending(
            call,
            HumanDecision::Rejected { reason: "no thanks".into() },
            Arc::new(Catalog),
            &h.params(),
        )
        .await
        .unwrap();

    assert_eq!(*h.shell.lock().unwrap(), 0);
    let call = h.call(call).await;
    assert_eq!(call.state, CallState::Rejected);
    assert_eq!(call.result.as_deref(), Some("no thanks"));
}

#[tokio::test]
async fn resolving_an_already_terminal_call_is_a_no_op() {
    let h = H::new(vec![], vec![]).await;
    let call = h.interrupted_call(h.root, "counter").await;
    h.store
        .resolve_call(call, &agent_loop::store::CallOutcome::Cancelled)
        .await
        .unwrap();

    let report = h
        .manager
        .resolve_pending(call, HumanDecision::Approved, Arc::new(Catalog), &h.params())
        .await
        .unwrap();

    // Cancelled is terminal and never re-executed (blueprint §8.2).
    assert_eq!(*h.counter.lock().unwrap(), 0);
    assert_eq!(h.call(call).await.state, CallState::Cancelled);
    assert_eq!(report.frames_resumed, 0);
}
