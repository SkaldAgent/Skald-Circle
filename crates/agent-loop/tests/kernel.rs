//! Kernel test suite (blueprint §13) — against `FakeModel` + `InMemoryStore`,
//! no DB, no Docker, no network.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_loop::gate::DenyList;
use agent_loop::ids::ConversationId;
use agent_loop::kernel::TurnOutcome;
use agent_loop::manager::{LoopManager, TurnMeta, TurnParams};
use agent_loop::model::{ModelHint, StaticModels, StreamDelta};
use agent_loop::prelude::async_trait;
use agent_loop::store::{CallState, FrameSpec, HistoryStore, NewMessage};
use agent_loop::store_memory::InMemoryStore;
use agent_loop::testing::{self, FakeModel, Step};
use agent_loop::tool::{Tool, ToolCtx, ToolFailure, ToolOutput, ToolRegistry};
use agent_loop::context::StaticSystemContext;
use agent_loop::delegate::{AgentCatalog, AgentKind, AgentProfile, DelegateTool, ToolSelection};
use agent_loop::events::LoopEvent;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

// ── test tools ──

struct WeatherTool;

#[async_trait]
impl Tool for WeatherTool {
    fn name(&self) -> &str { "get_weather" }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{"name":"get_weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}}}})
    }
    async fn call(&self, args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        Ok(ToolOutput::Text(format!("Sunny in {}", args["city"].as_str().unwrap_or("?"))))
    }
}

struct SlowTool;

#[async_trait]
impl Tool for SlowTool {
    fn name(&self) -> &str { "slow" }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{"name":"slow","parameters":{"type":"object"}}})
    }
    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok(ToolOutput::Text("done".into()))
    }
}

/// Concurrency-safe tool rendezvousing on a barrier: proves the fan-out runs
/// concurrently (a sequential path would deadlock → timeout).
struct BarrierTool {
    name:    &'static str,
    barrier: Arc<tokio::sync::Barrier>,
    log:     Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for BarrierTool {
    fn name(&self) -> &str { self.name }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{"name":self.name,"parameters":{"type":"object"}}})
    }
    fn concurrency_safe(&self, _args: &Value) -> bool { true }
    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        self.log.lock().unwrap().push(format!("start:{}", self.name));
        self.barrier.wait().await;
        self.log.lock().unwrap().push(format!("end:{}", self.name));
        Ok(ToolOutput::Text(format!("{} done", self.name)))
    }
}

/// Records start/end order in a shared log (sequentiality proofs).
struct OrderedTool {
    name: &'static str,
    safe: bool,
    log:  Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for OrderedTool {
    fn name(&self) -> &str { self.name }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{"name":self.name,"parameters":{"type":"object"}}})
    }
    fn concurrency_safe(&self, _args: &Value) -> bool { self.safe }
    async fn call(&self, _args: Value, _ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        self.log.lock().unwrap().push(format!("start:{}", self.name));
        tokio::task::yield_now().await;
        self.log.lock().unwrap().push(format!("end:{}", self.name));
        Ok(ToolOutput::Text("ok".into()))
    }
}

/// Marks itself AwaitingHuman then suspends (ask_user semantics).
struct SuspendTool {
    store: Arc<InMemoryStore>,
}

#[async_trait]
impl Tool for SuspendTool {
    fn name(&self) -> &str { "suspend_me" }
    fn definition(&self) -> Value {
        json!({"type":"function","function":{"name":"suspend_me","parameters":{"type":"object"}}})
    }
    async fn call(&self, _args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        self.store
            .set_call_state(ctx.call_id, CallState::AwaitingHuman)
            .await
            .map_err(|e| ToolFailure::Failed(e.to_string()))?;
        Err(ToolFailure::Suspend)
    }
}

// ── harness ──

struct Harness {
    manager: LoopManager,
    store:   Arc<InMemoryStore>,
}

fn harness_with(model: testing::FakeModel) -> Harness {
    let store = Arc::new(InMemoryStore::new());
    let manager = LoopManager::builder()
        .models(Arc::new(agent_loop::model::SingleModel::new(model)))
        .store(store.clone())
        .build()
        .unwrap();
    Harness { manager, store }
}

async fn params(
    manager: &LoopManager,
    conv: &ConversationId,
    tools: Arc<dyn agent_loop::tool::ToolSet>,
) -> TurnParams {
    let frame = manager.open_root(conv, FrameSpec::root("assistant")).await.unwrap();
    TurnParams {
        frame,
        agent: "assistant".into(),
        system: Arc::new(StaticSystemContext::new("You are a test agent.")),
        tools,
        model_hint: ModelHint::default(),
        selector: None,
        live_input: None,
        extensions: Default::default(),
        meta: TurnMeta::default(),
        assembler: None,
    }
}

// ── tests ──

#[tokio::test]
async fn multi_round_text_tool_text_final() {
    let model = FakeModel::new("m", vec![
        Step::tool_calls("let me check", vec![testing::call("c1", "get_weather", json!({"city":"Rome"}))]),
        Step::message("It is sunny in Rome."),
    ]);
    let h = harness_with(model);
    let conv = ConversationId::new("t1");
    let tools = ToolRegistry::new().with(WeatherTool).into_toolset();
    let p = params(&h.manager, &conv, tools).await;

    let handle = h.manager.start_turn(conv, NewMessage::user("weather?"), p).await.unwrap();
    let outcome = handle.join().await.unwrap();

    let TurnOutcome::Final { content, .. } = outcome else { panic!("expected Final, got {outcome:?}") };
    assert_eq!(content, "It is sunny in Rome.");

    // The store recorded everything: user, assistant+tool_call, tool result,
    // final assistant.
    let frame = h.manager.store().active_frames(&ConversationId::new("t1")).await.unwrap()[0].id;
    let history = h.store.load(frame).await.unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[1].calls.len(), 1);
    assert_eq!(history[1].calls[0].state, CallState::Done);
    assert_eq!(history[1].calls[0].result.as_deref(), Some("Sunny in Rome"));
}

#[tokio::test]
async fn exhausted_after_max_rounds() {
    let model = FakeModel::new("m", vec![
        Step::tool_calls("", vec![testing::call("c1", "get_weather", json!({}))]),
        Step::tool_calls("", vec![testing::call("c2", "get_weather", json!({}))]),
    ]);
    let store = Arc::new(InMemoryStore::new());
    let manager = LoopManager::builder()
        .models(Arc::new(agent_loop::model::SingleModel::new(model)))
        .store(store.clone())
        .max_rounds(2)
        .build()
        .unwrap();
    let conv = ConversationId::new("t2");
    let p = params(&manager, &conv, ToolRegistry::new().with(WeatherTool).into_toolset()).await;

    let handle = manager.start_turn(conv, NewMessage::user("loop forever"), p).await.unwrap();
    let outcome = handle.join().await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Exhausted), "got {outcome:?}");
}

#[tokio::test]
async fn fallback_retriable_moves_to_second_model() {
    let m1 = Arc::new(FakeModel::new("m1", vec![Step::error(Some(500), "boom")]));
    let m2 = Arc::new(FakeModel::new("m2", vec![Step::message("recovered")]));
    let store = Arc::new(InMemoryStore::new());
    let mut rx;
    let manager = LoopManager::builder()
        .models(Arc::new(StaticModels::new(vec![
            testing::handle(&m1, "m1"),
            testing::handle(&m2, "m2"),
        ])))
        .store(store.clone())
        .build()
        .unwrap();
    rx = manager.events();
    let conv = ConversationId::new("t3");
    let p = params(&manager, &conv, ToolRegistry::new().into_toolset()).await;

    let handle = manager.start_turn(conv, NewMessage::user("hi"), p).await.unwrap();
    let outcome = handle.join().await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Final { .. }), "got {outcome:?}");

    assert_eq!(m1.requests().len(), 1);
    assert_eq!(m2.requests().len(), 1);

    let mut saw_fallback = false;
    while let Ok(ev) = rx.try_recv() {
        if let LoopEvent::ModelFallback { from, to, .. } = ev.inner {
            assert_eq!(from, "m1");
            assert_eq!(to, "m2");
            saw_fallback = true;
        }
    }
    assert!(saw_fallback, "no ModelFallback event");
}

#[tokio::test]
async fn non_retriable_error_stops_without_fallback() {
    let m1 = Arc::new(FakeModel::new("m1", vec![Step::error(Some(404), "no such model")]));
    let m2 = Arc::new(FakeModel::new("m2", vec![Step::message("never reached")]));
    let store = Arc::new(InMemoryStore::new());
    let manager = LoopManager::builder()
        .models(Arc::new(StaticModels::new(vec![
            testing::handle(&m1, "m1"),
            testing::handle(&m2, "m2"),
        ])))
        .store(store.clone())
        .build()
        .unwrap();
    let conv = ConversationId::new("t4");
    let p = params(&manager, &conv, ToolRegistry::new().into_toolset()).await;

    let handle = manager.start_turn(conv, NewMessage::user("hi"), p).await.unwrap();
    assert!(handle.join().await.is_err(), "404 must fail the turn");
    assert_eq!(m2.requests().len(), 0, "404 must not fall back");
}

#[tokio::test]
async fn cancel_during_llm_call() {
    let model = FakeModel::new("m", vec![Step::pending()]);
    let h = harness_with(model);
    let conv = ConversationId::new("t5");
    let p = params(&h.manager, &conv, ToolRegistry::new().into_toolset()).await;

    let handle = h.manager.start_turn(conv.clone(), NewMessage::user("hi"), p).await.unwrap();
    let cancel: CancellationToken = handle.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.cancel();
    });
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("join hung")
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Cancelled), "got {outcome:?}");
    assert!(!h.manager.is_running(&conv));
}

#[tokio::test]
async fn cancel_during_slow_tool_marks_call_cancelled() {
    let model = FakeModel::new("m", vec![
        Step::tool_calls("", vec![testing::call("c1", "slow", json!({}))]),
    ]);
    let h = harness_with(model);
    let conv = ConversationId::new("t6");
    let p = params(&h.manager, &conv, ToolRegistry::new().with(SlowTool).into_toolset()).await;
    let frame = p.frame;

    let handle = h.manager.start_turn(conv, NewMessage::user("run slow"), p).await.unwrap();
    let cancel = handle.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel.cancel();
    });
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("join hung")
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Cancelled), "got {outcome:?}");

    let calls = h.store.calls_in_state(frame, &[CallState::Cancelled]).await.unwrap();
    assert_eq!(calls.len(), 1, "the slow call must be recorded Cancelled, got {calls:?}");
}

#[tokio::test]
async fn fan_out_runs_concurrently_and_records_in_order() {
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let log = Arc::new(Mutex::new(Vec::new()));
    let model = FakeModel::new("m", vec![
        Step::tool_calls("", vec![
            testing::call("c1", "p1", json!({})),
            testing::call("c2", "p2", json!({})),
            testing::call("c3", "p3", json!({})),
        ]),
        Step::message("all done"),
    ]);
    let h = harness_with(model);
    let conv = ConversationId::new("t7");
    let p = params(&h.manager, &conv, ToolRegistry::new()
        .with_arc(Arc::new(BarrierTool { name: "p1", barrier: barrier.clone(), log: log.clone() }))
        .with_arc(Arc::new(BarrierTool { name: "p2", barrier: barrier.clone(), log: log.clone() }))
        .with_arc(Arc::new(BarrierTool { name: "p3", barrier: barrier.clone(), log: log.clone() }))
        .into_toolset()).await;
    let frame = p.frame;

    let handle = h.manager.start_turn(conv, NewMessage::user("go"), p).await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("fan-out deadlocked (ran sequentially?)")
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Final { .. }), "got {outcome:?}");

    // All three started before any ended (true concurrency).
    {
        let log = log.lock().unwrap();
        let first_end = log.iter().position(|e| e.starts_with("end:")).unwrap();
        assert_eq!(log[..first_end].iter().filter(|e| e.starts_with("start:")).count(), 3,
            "not all tools started before the first end: {log:?}");
    }

    // Ids are increasing in call order and all resolved Done.
    let calls = h.store.calls_in_state(frame, &[CallState::Done]).await.unwrap();
    assert_eq!(calls.len(), 3);
    let mut ids: Vec<i64> = calls.iter().map(|c| c.id.get()).collect();
    let sorted = ids.clone();
    ids.sort_unstable();
    // calls_in_state returns in message order; ids must already be ascending.
    assert_eq!(ids, sorted);
}

#[tokio::test]
async fn mixed_batch_stays_sequential() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let model = FakeModel::new("m", vec![
        Step::tool_calls("", vec![
            testing::call("c1", "safe", json!({})),
            testing::call("c2", "unsafe", json!({})),
        ]),
        Step::message("done"),
    ]);
    let h = harness_with(model);
    let conv = ConversationId::new("t8");
    let p = params(&h.manager, &conv, ToolRegistry::new()
        .with_arc(Arc::new(OrderedTool { name: "safe", safe: true, log: log.clone() }))
        .with_arc(Arc::new(OrderedTool { name: "unsafe", safe: false, log: log.clone() }))
        .into_toolset()).await;

    let handle = h.manager.start_turn(conv, NewMessage::user("go"), p).await.unwrap();
    let outcome = handle.join().await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Final { .. }), "got {outcome:?}");

    assert_eq!(
        *log.lock().unwrap(),
        vec!["start:safe", "end:safe", "start:unsafe", "end:unsafe"],
        "mixed batch must run sequentially in order"
    );
}

#[tokio::test]
async fn suspend_leaves_call_awaiting_human_and_ends_turn() {
    let store = Arc::new(InMemoryStore::new());
    let model = FakeModel::new("m", vec![
        Step::tool_calls("", vec![testing::call("c1", "suspend_me", json!({}))]),
    ]);
    let manager = LoopManager::builder()
        .models(Arc::new(agent_loop::model::SingleModel::new(model)))
        .store(store.clone())
        .build()
        .unwrap();
    let conv = ConversationId::new("t9");
    let suspend = SuspendTool { store: store.clone() };
    let p = params(&manager, &conv, ToolRegistry::new().with(suspend).into_toolset()).await;
    let frame = p.frame;

    let handle = manager.start_turn(conv, NewMessage::user("ask something"), p).await.unwrap();
    let outcome = handle.join().await.unwrap();
    assert!(matches!(outcome, TurnOutcome::Cancelled), "got {outcome:?}");

    let pending = store.calls_in_state(frame, &[CallState::AwaitingHuman]).await.unwrap();
    assert_eq!(pending.len(), 1, "the call must STAY AwaitingHuman");
    assert!(pending[0].result.is_none(), "no result recorded for a suspended call");
}

#[tokio::test]
async fn gate_reject_marks_rejected_and_loop_continues() {
    let model = FakeModel::new("m", vec![
        Step::tool_calls("", vec![testing::call("c1", "blocked_tool", json!({}))]),
        Step::message("after rejection"),
    ]);
    let store = Arc::new(InMemoryStore::new());
    let manager = LoopManager::builder()
        .models(Arc::new(agent_loop::model::SingleModel::new(model)))
        .store(store.clone())
        .gate(DenyList::new(["blocked_*"]))
        .build()
        .unwrap();
    let conv = ConversationId::new("t10");
    let p = params(&manager, &conv, ToolRegistry::new().with(WeatherTool).into_toolset()).await;
    let frame = p.frame;

    let handle = manager.start_turn(conv, NewMessage::user("try it"), p).await.unwrap();
    let outcome = handle.join().await.unwrap();
    let TurnOutcome::Final { content, .. } = outcome else { panic!("expected Final, got {outcome:?}") };
    assert_eq!(content, "after rejection");

    let rejected = store.calls_in_state(frame, &[CallState::Rejected]).await.unwrap();
    assert_eq!(rejected.len(), 1);
}

#[tokio::test]
async fn streaming_deltas_precede_outcome_events() {
    let model = FakeModel::new("m", vec![
        Step::message("hello").with_deltas(vec![
            StreamDelta::Text("he".into()),
            StreamDelta::Text("llo".into()),
        ]),
    ]);
    let h = harness_with(model);
    let mut rx = h.manager.events();
    let conv = ConversationId::new("t11");
    let p = params(&h.manager, &conv, ToolRegistry::new().into_toolset()).await;

    let handle = h.manager.start_turn(conv, NewMessage::user("hi"), p).await.unwrap();
    let _ = handle.join().await.unwrap();

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev.inner);
    }
    let done_idx = events.iter().position(|e| matches!(e, LoopEvent::Done { .. })).unwrap();
    let delta_count = events[..done_idx]
        .iter()
        .filter(|e| matches!(e, LoopEvent::TokenDelta { .. }))
        .count();
    assert_eq!(delta_count, 2, "both deltas must precede Done: {events:?}");
}

#[tokio::test]
async fn orphan_user_message_marked_failed_on_new_turn() {
    let model = FakeModel::new("m", vec![Step::message("reply")]);
    let h = harness_with(model);
    let conv = ConversationId::new("t12");
    let p = params(&h.manager, &conv, ToolRegistry::new().into_toolset()).await;
    let frame = p.frame;

    // A previous user message with no assistant reply (crash mid-turn).
    h.store.append(frame, NewMessage::user("orphan")).await.unwrap();

    let handle = h.manager.start_turn(conv, NewMessage::user("fresh"), p).await.unwrap();
    let _ = handle.join().await.unwrap();

    let history = h.store.load(frame).await.unwrap();
    assert!(
        !history.iter().any(|m| m.content == "orphan"),
        "the orphan must be excluded from the projection: {history:?}"
    );
}

#[tokio::test]
async fn second_loop_on_same_conversation_rejected() {
    let model = FakeModel::new("m", vec![Step::pending()]);
    let h = harness_with(model);
    let conv = ConversationId::new("t13");
    let p1 = params(&h.manager, &conv, ToolRegistry::new().into_toolset()).await;

    let handle = h.manager.start_turn(conv.clone(), NewMessage::user("first"), p1).await.unwrap();

    let p2 = params(&h.manager, &conv, ToolRegistry::new().into_toolset()).await;
    let second = h.manager.start_turn(conv.clone(), NewMessage::user("second"), p2).await;
    assert!(
        matches!(second, Err(agent_loop::manager::StartError::AlreadyRunning)),
        "double-driving must be rejected"
    );

    handle.cancel.cancel();
    let _ = handle.join().await;
}

// ── delegate (sub-agents as a tool) ──

struct TestCatalog {
    context: Arc<StaticSystemContext>,
    /// Pins the child to its own model, so a test can script parent and child
    /// independently (a shared script would race on who pops which step).
    model:   Option<ModelHint>,
}

#[async_trait]
impl AgentCatalog for TestCatalog {
    async fn get(
        &self,
        id:           &str,
        _child_frame: agent_loop::ids::FrameId,
        _ctx:         &agent_loop::tool::ToolCtx,
    ) -> agent_loop::Result<AgentProfile> {
        Ok(AgentProfile {
            id: id.into(),
            kind: AgentKind::Task,
            context: self.context.clone(),
            tools: ToolSelection::inherit(),
            toolset: None,
            model: self.model.clone(),
            selector: None,
            assembler: None,
        })
    }
    async fn list(&self, _kind: AgentKind) -> Vec<agent_loop::delegate::AgentSummary> {
        Vec::new()
    }
}

#[tokio::test]
async fn sync_delegate_runs_child_loop_and_returns_result() {
    let script = vec![
        Step::tool_calls("delegating", vec![testing::call("c1", "delegate", json!({"agent_id":"researcher","prompt":"find X"}))]),
        Step::message("research says: X=42"),
        Step::message("final answer with X=42"),
    ];
    let store = Arc::new(InMemoryStore::new());
    let manager = Arc::new(
        LoopManager::builder()
            .models(Arc::new(agent_loop::model::SingleModel::new(FakeModel::new("m", script))))
            .store(store.clone())
            .build()
            .unwrap(),
    );
    let catalog: Arc<dyn AgentCatalog> = Arc::new(TestCatalog {
        context: Arc::new(StaticSystemContext::new("You are a researcher.")),
        model:   None,
    });
    let delegate: Arc<dyn Tool> = Arc::new(DelegateTool::new(manager.clone(), catalog, manager.store(), 5));

    let conv = ConversationId::new("d1");
    let frame = manager.open_root(&conv, FrameSpec::root("assistant")).await.unwrap();
    let p = TurnParams {
        frame,
        agent: "assistant".into(),
        system: Arc::new(StaticSystemContext::new("root")),
        tools: ToolRegistry::new().with_arc(delegate).into_toolset(),
        model_hint: ModelHint::default(),
        selector: None,
        live_input: None,
        extensions: Default::default(),
        meta: TurnMeta::default(),
        assembler: None,
    };

    let handle = manager.start_turn(conv.clone(), NewMessage::user("what is X?"), p).await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("delegate turn hung")
        .unwrap();
    let TurnOutcome::Final { content, .. } = outcome else { panic!("expected Final, got {outcome:?}") };
    assert_eq!(content, "final answer with X=42");

    // The parent's delegate call resolved Done with the CHILD's answer as result.
    let done = store.calls_in_state(frame, &[CallState::Done]).await.unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].result.as_deref(), Some("research says: X=42"));

    // The child frame exists, closed, with its Agent prompt + assistant answer.
    let frames = store.active_frames(&conv).await.unwrap();
    assert!(frames.iter().all(|f| f.spec.depth == 0), "child frame must be closed");
    let history_all = store.load(frame).await.unwrap();
    assert!(history_all.iter().any(|m| m.role == agent_loop::store::Role::Assistant && m.content == "final answer with X=42"));
}

#[tokio::test]
async fn delegate_batch_fans_out_concurrently() {
    let script = vec![
        Step::tool_calls("", vec![
            testing::call("c1", "delegate", json!({"agent_id":"a1","prompt":"job one"})),
            testing::call("c2", "delegate", json!({"agent_id":"a2","prompt":"job two"})),
        ]),
        Step::message("result one"),
        Step::message("result two"),
        Step::message("both done"),
    ];
    let store = Arc::new(InMemoryStore::new());
    let manager = Arc::new(
        LoopManager::builder()
            .models(Arc::new(agent_loop::model::SingleModel::new(FakeModel::new("m", script))))
            .store(store.clone())
            .max_parallel_calls(2)
            .build()
            .unwrap(),
    );
    let catalog: Arc<dyn AgentCatalog> = Arc::new(TestCatalog {
        context: Arc::new(StaticSystemContext::new("worker")),
        model:   None,
    });
    let delegate: Arc<dyn Tool> = Arc::new(DelegateTool::new(manager.clone(), catalog, manager.store(), 5));

    let conv = ConversationId::new("d2");
    let frame = manager.open_root(&conv, FrameSpec::root("assistant")).await.unwrap();
    let p = TurnParams {
        frame,
        agent: "assistant".into(),
        system: Arc::new(StaticSystemContext::new("root")),
        tools: ToolRegistry::new().with_arc(delegate).into_toolset(),
        model_hint: ModelHint::default(),
        selector: None,
        live_input: None,
        extensions: Default::default(),
        meta: TurnMeta::default(),
        assembler: None,
    };

    let handle = manager.start_turn(conv, NewMessage::user("do both"), p).await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("delegate batch hung")
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Final { .. }), "got {outcome:?}");

    // Both delegate calls resolved Done, each carrying one of the child results.
    let done = store.calls_in_state(frame, &[CallState::Done]).await.unwrap();
    assert_eq!(done.len(), 2);
    let results: HashSet<String> = done.iter().filter_map(|c| c.result.clone()).collect();
    assert_eq!(
        results,
        ["result one".to_string(), "result two".to_string()].into_iter().collect()
    );
}

// ── async delegation ──

/// Polls until `f` holds, so a background delivery does not need a sleep.
async fn eventually<F, Fut>(label: &str, f: F)
where
    F:   Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {label}");
}

#[tokio::test]
async fn async_delegate_returns_a_receipt_then_delivers_the_result() {
    // Parent and child get their own scripted model: the parent does NOT wait
    // for the child, so one shared script would race on who pops which step.
    let root = Arc::new(FakeModel::new("root", vec![
        Step::tool_calls("", vec![testing::call("c1", "delegate", json!({
            "agent_id": "worker", "prompt": "long job", "mode": "async", "title": "nightly",
        }))]),
        Step::message("started it"),
    ]));
    let child = Arc::new(FakeModel::new("child", vec![Step::message("the long answer")]));

    let store = Arc::new(InMemoryStore::new());
    let manager = Arc::new(
        LoopManager::builder()
            .models(Arc::new(StaticModels::new(vec![
                testing::handle(&root, "root"),
                testing::handle(&child, "child"),
            ])))
            .store(store.clone())
            .build()
            .unwrap(),
    );
    let catalog: Arc<dyn AgentCatalog> = Arc::new(TestCatalog {
        context: Arc::new(StaticSystemContext::new("worker")),
        model:   Some(ModelHint::name("child")),
    });
    let sink: Arc<dyn AsyncResultSink> = Arc::new(StoreSink::new(manager.store()));
    let exec: Arc<dyn AsyncExecutor> = Arc::new(InProcessExecutor::new(
        manager.clone(),
        catalog.clone(),
        manager.store(),
        sink,
        ToolRegistry::new().into_toolset(),
    ));
    let delegate: Arc<dyn Tool> = Arc::new(
        DelegateTool::new(manager.clone(), catalog, manager.store(), 5).with_async(exec),
    );

    let conv = ConversationId::new("d3");
    let frame = manager.open_root(&conv, FrameSpec::root("assistant")).await.unwrap();
    let p = TurnParams {
        frame,
        agent: "assistant".into(),
        system: Arc::new(StaticSystemContext::new("root")),
        tools: ToolRegistry::new().with_arc(delegate).into_toolset(),
        model_hint: ModelHint::default(),
        selector: None,
        live_input: None,
        extensions: Default::default(),
        meta: TurnMeta::default(),
        assembler: None,
    };

    let handle = manager.start_turn(conv.clone(), NewMessage::user("run it"), p).await.unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("async delegate must not block the parent turn")
        .unwrap();
    let TurnOutcome::Final { content, .. } = outcome else { panic!("got {outcome:?}") };
    assert_eq!(content, "started it");

    // The delegating call resolved with a receipt, not with the child's answer.
    let done = store.calls_in_state(frame, &[CallState::Done]).await.unwrap();
    let receipt: Value =
        serde_json::from_str(done[0].result.as_deref().unwrap()).expect("receipt is JSON");
    assert_eq!(receipt["status"], "started");
    assert_eq!(receipt["task_id"], 1);

    // …and the answer lands later, as its own completed call.
    let store_c = store.clone();
    eventually("the delivered result", || {
        let store = store_c.clone();
        async move {
            store
                .load(frame)
                .await
                .unwrap()
                .iter()
                .any(|m| m.calls.iter().any(|c| c.name == agent_loop::delegate::DELIVERY_CALL))
        }
    })
    .await;

    let history = store.load(frame).await.unwrap();
    let delivery = history
        .iter()
        .find(|m| m.calls.iter().any(|c| c.name == agent_loop::delegate::DELIVERY_CALL))
        .unwrap();
    assert!(delivery.synthetic, "the delivery is not a turn the user drove");
    let call = &delivery.calls[0];
    assert_eq!(call.state, CallState::Done);
    let payload: Value = serde_json::from_str(call.result.as_deref().unwrap()).unwrap();
    assert_eq!(payload["task_id"], 1);
    assert_eq!(payload["title"], "nightly");
    assert_eq!(payload["result"], "the long answer");
}

#[tokio::test]
async fn async_delegate_without_an_executor_is_refused() {
    let script = vec![
        Step::tool_calls("", vec![testing::call("c1", "delegate", json!({
            "agent_id": "worker", "prompt": "job", "mode": "async",
        }))]),
        Step::message("could not start it"),
    ];
    let store = Arc::new(InMemoryStore::new());
    let manager = Arc::new(
        LoopManager::builder()
            .models(Arc::new(agent_loop::model::SingleModel::new(FakeModel::new("m", script))))
            .store(store.clone())
            .build()
            .unwrap(),
    );
    let catalog: Arc<dyn AgentCatalog> = Arc::new(TestCatalog {
        context: Arc::new(StaticSystemContext::new("worker")),
        model:   None,
    });
    // No `with_async`: the mode must fail, never silently run sync — a turn
    // that asked not to wait would otherwise block on the child.
    let delegate: Arc<dyn Tool> =
        Arc::new(DelegateTool::new(manager.clone(), catalog, manager.store(), 5));

    let conv = ConversationId::new("d4");
    let frame = manager.open_root(&conv, FrameSpec::root("assistant")).await.unwrap();
    let p = TurnParams {
        frame,
        agent: "assistant".into(),
        system: Arc::new(StaticSystemContext::new("root")),
        tools: ToolRegistry::new().with_arc(delegate).into_toolset(),
        model_hint: ModelHint::default(),
        selector: None,
        live_input: None,
        extensions: Default::default(),
        meta: TurnMeta::default(),
        assembler: None,
    };

    let handle = manager.start_turn(conv.clone(), NewMessage::user("run it"), p).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("turn hung")
        .unwrap();

    let failed = store.calls_in_state(frame, &[CallState::Failed]).await.unwrap();
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0].result.as_deref().unwrap().contains("async mode is not available"),
        "{:?}",
        failed[0].result
    );
    // Nothing was spawned: no child frame was ever opened.
    assert!(store.active_frames(&conv).await.unwrap().iter().all(|f| f.spec.depth == 0));
}

use agent_loop::delegate::{AsyncExecutor, AsyncResultSink, InProcessExecutor, StoreSink};
use std::collections::HashSet;
