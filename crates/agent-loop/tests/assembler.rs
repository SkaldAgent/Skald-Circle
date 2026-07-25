//! Assembler tests (blueprint §13): well-formed projection, DTL rendering
//! modes, summary, window, crash survivors.

use std::sync::Arc;

use agent_loop::activation::{Activation, ActivationSource, ToolRendering};
use agent_loop::context::{AssembleInput, ContextAssembler, LinearAssembler, SystemContext};
use agent_loop::ids::{ConversationId, FrameId};
use agent_loop::model::ModelInfo;
use agent_loop::prelude::async_trait;
use agent_loop::store::{
    CallOutcome, FrameSpec, HistoryStore, NewCall, NewMessage,
};
use agent_loop::store_memory::InMemoryStore;
use agent_loop::tool::ToolOutput;
use serde_json::{Value, json};

fn tool_def(name: &str) -> Value {
    json!({"type":"function","function":{"name":name,"parameters":{"type":"object"}}})
}

struct StubActivations {
    acts: Vec<Activation>,
}

#[async_trait]
impl ActivationSource for StubActivations {
    async fn activations(&self, _frame: FrameId) -> agent_loop::Result<Vec<Activation>> {
        Ok(self.acts.clone())
    }
}

fn model_info(mode: ToolRendering) -> ModelInfo {
    ModelInfo { tool_rendering: mode, ..ModelInfo::default() }
}

async fn input(store: &Arc<InMemoryStore>, conv: &ConversationId, mode: ToolRendering) -> (FrameId, AssembleInput) {
    let frame = store.open_frame(conv, None, FrameSpec::root("assistant")).await.unwrap();
    let input = AssembleInput {
        frame,
        system: SystemContext::base("BASE"),
        model: model_info(mode),
        round: 0,
    };
    (frame, input)
}

/// History: user → assistant with an activate_tools call (resolved) → final.
/// Returns the anchor (the assistant message id).
async fn seed_activation_history(store: &Arc<InMemoryStore>, frame: FrameId) -> agent_loop::ids::MessageId {
    store.append(frame, NewMessage::user("use gmail")).await.unwrap();
    let anchor = store.append(frame, NewMessage::assistant("activating", None)).await.unwrap();
    let call = store
        .append_call(anchor, NewCall::new("activate_tools", json!({"groups":["gmail"]})).with_provider_id("c1"))
        .await
        .unwrap();
    store
        .resolve_call(call, &CallOutcome::Completed(ToolOutput::Text("gmail activated".into())))
        .await
        .unwrap();
    anchor
}

#[tokio::test]
async fn inline_mode_injects_nothing() {
    let store = Arc::new(InMemoryStore::new());
    let conv = ConversationId::new("a1");
    let (frame, input) = input(&store, &conv, ToolRendering::Inline).await;
    let anchor = seed_activation_history(&store, frame).await;

    let assembler = LinearAssembler::new().with_activation(Arc::new(StubActivations {
        acts: vec![Activation { anchor, defs: vec![tool_def("mcp__gmail__send")] }],
    }));
    let store_dyn: Arc<dyn HistoryStore> = store;
    let msgs = assembler.build(&store_dyn, &input).await.unwrap();

    assert!(!msgs.iter().any(|m| m.get("tools").is_some()), "Inline must not inject system+tools");
    assert!(!msgs.iter().any(|m| m.get("_tool_references").is_some()));
}

#[tokio::test]
async fn system_tool_block_appends_after_tool_results() {
    let store = Arc::new(InMemoryStore::new());
    let conv = ConversationId::new("a2");
    let (frame, input) = input(&store, &conv, ToolRendering::SystemToolBlock).await;
    let anchor = seed_activation_history(&store, frame).await;

    let assembler = LinearAssembler::new().with_activation(Arc::new(StubActivations {
        acts: vec![Activation { anchor, defs: vec![tool_def("mcp__gmail__send")] }],
    }));
    let store_dyn: Arc<dyn HistoryStore> = store;
    let msgs = assembler.build(&store_dyn, &input).await.unwrap();

    // [system BASE, user, assistant(tool_calls), tool(result), system+tools]
    let block_idx = msgs
        .iter()
        .position(|m| m["role"].as_str() == Some("system") && m.get("tools").is_some())
        .expect("no system+tools block injected");
    assert_eq!(msgs[block_idx]["tools"][0]["function"]["name"], json!("mcp__gmail__send"));
    assert!(msgs[block_idx].get("content").is_none(), "Kimi block has no content field");
    // It comes right after the tool result of the anchor group.
    assert_eq!(msgs[block_idx - 1]["role"], json!("tool"));
}

#[tokio::test]
async fn deferred_tool_reference_marks_first_tool_result() {
    let store = Arc::new(InMemoryStore::new());
    let conv = ConversationId::new("a3");
    let (frame, input) = input(&store, &conv, ToolRendering::DeferredToolReference).await;
    let anchor = seed_activation_history(&store, frame).await;

    let assembler = LinearAssembler::new().with_activation(Arc::new(StubActivations {
        acts: vec![Activation { anchor, defs: vec![tool_def("mcp__gmail__send")] }],
    }));
    let store_dyn: Arc<dyn HistoryStore> = store;
    let msgs = assembler.build(&store_dyn, &input).await.unwrap();

    let tool_msg = msgs
        .iter()
        .find(|m| m["role"].as_str() == Some("tool"))
        .expect("no tool result projected");
    assert_eq!(tool_msg["_tool_references"], json!(["mcp__gmail__send"]));
}

#[tokio::test]
async fn crash_survivors_get_synthetic_interrupted_results() {
    let store = Arc::new(InMemoryStore::new());
    let conv = ConversationId::new("a4");
    let (frame, input) = input(&store, &conv, ToolRendering::Inline).await;

    store.append(frame, NewMessage::user("do it")).await.unwrap();
    let msg = store.append(frame, NewMessage::assistant("running", None)).await.unwrap();
    // Never resolved: still Running, as after a crash.
    store.append_call(msg, NewCall::new("execute_cmd", json!({})).with_provider_id("c1")).await.unwrap();

    let assembler = LinearAssembler::new();
    let store_dyn: Arc<dyn HistoryStore> = store;
    let msgs = assembler.build(&store_dyn, &input).await.unwrap();

    let tool_msg = msgs.iter().find(|m| m["role"].as_str() == Some("tool")).unwrap();
    assert!(
        tool_msg["content"].as_str().unwrap().contains("interrupted"),
        "a Running survivor must project a synthetic interrupted result: {tool_msg}"
    );
}

#[tokio::test]
async fn summary_replaces_covered_history() {
    let store = Arc::new(InMemoryStore::new());
    let conv = ConversationId::new("a5");
    let (frame, input) = input(&store, &conv, ToolRendering::Inline).await;

    let m1 = store.append(frame, NewMessage::user("old question")).await.unwrap();
    store.append(frame, NewMessage::assistant("old answer", None)).await.unwrap();
    let m3 = store.append(frame, NewMessage::user("new question")).await.unwrap();

    store
        .save_summary(frame, agent_loop::store::NewSummary {
            text: "User asked about old stuff.".into(),
            covered_up_to: m1,
        })
        .await
        .unwrap();

    let assembler = LinearAssembler::new();
    let store_dyn: Arc<dyn HistoryStore> = store;
    let msgs = assembler.build(&store_dyn, &input).await.unwrap();

    let joined = msgs.iter().filter_map(|m| m["content"].as_str()).collect::<Vec<_>>().join("\n");
    assert!(joined.contains("CONTEXT SUMMARY"), "summary block missing: {joined}");
    assert!(joined.contains("old answer"), "post-summary messages must survive");
    assert!(!joined.contains("old question"), "covered messages must be gone");
    let _ = m3;
}

#[tokio::test]
async fn window_cuts_at_user_boundary_never_mid_tool_group() {
    let store = Arc::new(InMemoryStore::new());
    let conv = ConversationId::new("a6");
    let (frame, input) = input(&store, &conv, ToolRendering::Inline).await;

    store.append(frame, NewMessage::user("first")).await.unwrap();
    let asst = store.append(frame, NewMessage::assistant("calling", None)).await.unwrap();
    let call = store.append_call(asst, NewCall::new("t", json!({})).with_provider_id("c1")).await.unwrap();
    store.resolve_call(call, &CallOutcome::Completed(ToolOutput::Text("r".into()))).await.unwrap();
    store.append(frame, NewMessage::user("second")).await.unwrap();

    // Window of 2 would cut right before the assistant+tool group; the
    // boundary rule must move the cut to "second".
    let assembler = LinearAssembler::new().with_max_messages(2);
    let store_dyn: Arc<dyn HistoryStore> = store;
    let msgs = assembler.build(&store_dyn, &input).await.unwrap();

    let roles: Vec<&str> = msgs.iter().filter_map(|m| m["role"].as_str()).collect();
    assert_eq!(roles, ["system", "user"], "cut must land on the user boundary: {roles:?}");
}
