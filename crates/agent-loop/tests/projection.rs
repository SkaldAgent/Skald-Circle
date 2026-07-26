//! Golden tests of the projection (blueprint §13): the exact wire shape of
//! every layer, for every provider knob. These assert full messages, not just
//! properties — a change in what a model receives must show up here.

use std::sync::Arc;

use agent_loop::activation::{Activation, ActivationSource, ToolRendering};
use agent_loop::context::{AssembleInput, ContextAssembler, LinearAssembler, SystemContext};
use agent_loop::ids::{ConversationId, FrameId, MessageId};
use agent_loop::model::ModelInfo;
use agent_loop::prelude::async_trait;
use agent_loop::projection::{
    MediaBlob, MediaSource, Projection, ReasoningEcho, ResultLimit, ToolResultDigest,
};
use agent_loop::store::{
    CallOutcome, FrameSpec, HistoryStore, NewCall, NewMessage, NewSummary, StoredCall,
    StoredMessage,
};
use agent_loop::store_memory::InMemoryStore;
use agent_loop::tool::ToolOutput;
use serde_json::{Value, json};

// ── fixtures ─────────────────────────────────────────────────────────────────

async fn store_and_frame(name: &str) -> (Arc<dyn HistoryStore>, FrameId) {
    let store = Arc::new(InMemoryStore::new());
    let conv = ConversationId::new(name);
    let frame = store.open_frame(&conv, None, FrameSpec::root("assistant")).await.unwrap();
    (store, frame)
}

fn input(frame: FrameId, system: SystemContext, model: ModelInfo) -> AssembleInput {
    AssembleInput { frame, system, model, round: 0 }
}

fn tool_def(name: &str) -> Value {
    json!({"type":"function","function":{"name":name,"parameters":{"type":"object"}}})
}

/// The Skald-flavoured configuration: every knob off the default, so the test
/// exercises the parameterization rather than the defaults.
fn strict() -> Projection {
    Projection {
        summary_suffix:         Some("[End of summary]".into()),
        interrupted_text:       "Error: tool call was interrupted.".into(),
        rejected_default:       "User rejected this tool call.".into(),
        cancelled_default:      "Tool call was cancelled by the user.".into(),
        reasoning_placeholder:  Some("(no reasoning recorded for this step)".into()),
        reasoning_echo:         ReasoningEcho::Both,
        activation_anchor_tool: Some("activate_tools".into()),
        ..Projection::default()
    }
}

struct Stub(Vec<Activation>);

#[async_trait]
impl ActivationSource for Stub {
    async fn activations(&self, _frame: FrameId) -> agent_loop::Result<Vec<Activation>> {
        Ok(self.0.clone())
    }
}

// ── system layers ────────────────────────────────────────────────────────────

#[tokio::test]
async fn prompt_cache_turns_the_static_prefix_into_a_cache_breakpoint() {
    let (store, frame) = store_and_frame("p1").await;

    let plain = LinearAssembler::new()
        .build(&store, &input(frame, SystemContext::base("BASE"), ModelInfo::default()))
        .await
        .unwrap();
    assert_eq!(plain[0], json!({ "role": "system", "content": "BASE" }));

    let cached = LinearAssembler::new()
        .build(&store, &input(frame, SystemContext::base("BASE"), ModelInfo {
            prompt_cache: true,
            ..ModelInfo::default()
        }))
        .await
        .unwrap();
    assert_eq!(
        cached[0],
        json!({
            "role": "system",
            "content": [{ "type": "text", "text": "BASE",
                          "cache_control": { "type": "ephemeral" } }],
        })
    );
}

#[tokio::test]
async fn static_and_dynamic_layers_land_on_their_sides_of_the_history() {
    let (store, frame) = store_and_frame("p2").await;
    store.append(frame, NewMessage::user("hi")).await.unwrap();

    let system = SystemContext::base("BASE")
        .with_static("FORMAT RULES")
        .with_static("<scratchpad/>")
        .with_dynamic("MEMORY")
        .with_dynamic("NOW")
        .with_reminder("REMEMBER");

    let msgs = LinearAssembler::new()
        .build(&store, &input(frame, system, ModelInfo::default()))
        .await
        .unwrap();

    assert_eq!(msgs, vec![
        json!({ "role": "system", "content": "BASE" }),
        json!({ "role": "system", "content": "FORMAT RULES" }),
        json!({ "role": "system", "content": "<scratchpad/>" }),
        json!({ "role": "user",   "content": "hi" }),
        // The dynamic layers are ONE trailing block, joined by the separator.
        json!({ "role": "system", "content": "MEMORY\n\n---\nNOW" }),
        json!({ "role": "system", "content": "REMEMBER" }),
    ]);
}

#[tokio::test]
async fn summary_replaces_covered_history_and_carries_its_suffix() {
    let (store, frame) = store_and_frame("p3").await;
    let m1 = store.append(frame, NewMessage::user("old question")).await.unwrap();
    store.append(frame, NewMessage::assistant("old answer", None)).await.unwrap();
    store.append(frame, NewMessage::user("new question")).await.unwrap();
    store
        .save_summary(frame, NewSummary { text: "They discussed old stuff.".into(), covered_up_to: m1 })
        .await
        .unwrap();

    let msgs = LinearAssembler::new()
        .with_projection(strict())
        .build(&store, &input(frame, SystemContext::base("BASE"), ModelInfo::default()))
        .await
        .unwrap();

    assert_eq!(
        msgs[1],
        json!({
            "role": "system",
            "content": "[CONTEXT SUMMARY — earlier messages were compacted into this summary]\n\n\
                        They discussed old stuff.\n\n[End of summary]",
        })
    );
    let joined = msgs.iter().filter_map(|m| m["content"].as_str()).collect::<Vec<_>>().join("|");
    assert!(joined.contains("old answer"), "history after the cut must survive");
    assert!(!joined.contains("old question"), "covered history must be gone");
}

#[tokio::test]
async fn the_window_never_opens_on_half_an_exchange() {
    let (store, frame) = store_and_frame("p4").await;
    store.append(frame, NewMessage::user("first")).await.unwrap();
    let asst = store.append(frame, NewMessage::assistant("calling", None)).await.unwrap();
    let call = store.append_call(asst, NewCall::new("t", json!({})).with_provider_id("c1")).await.unwrap();
    store.resolve_call(call, &CallOutcome::Completed(ToolOutput::Text("r".into()))).await.unwrap();
    store.append(frame, NewMessage::user("second")).await.unwrap();

    // A window of 2 would start on the assistant+tool group: it is dropped.
    let msgs = LinearAssembler::new()
        .with_max_messages(2)
        .build(&store, &input(frame, SystemContext::base("BASE"), ModelInfo::default()))
        .await
        .unwrap();

    let roles: Vec<&str> = msgs.iter().filter_map(|m| m["role"].as_str()).collect();
    assert_eq!(roles, ["system", "user"]);
}

// ── tool calls and results ───────────────────────────────────────────────────

/// Seeds one assistant turn with a call in each terminal state, plus a survivor.
async fn seed_states(store: &Arc<dyn HistoryStore>, frame: FrameId) -> MessageId {
    store.append(frame, NewMessage::user("go")).await.unwrap();
    let msg = store.append(frame, NewMessage::assistant("working", None)).await.unwrap();

    let done = store.append_call(msg, NewCall::new("a", json!({})).with_provider_id("c1")).await.unwrap();
    store.resolve_call(done, &CallOutcome::Completed(ToolOutput::Text("ok".into()))).await.unwrap();

    let failed = store.append_call(msg, NewCall::new("b", json!({})).with_provider_id("c2")).await.unwrap();
    store.resolve_call(failed, &CallOutcome::Failed("boom".into())).await.unwrap();

    let rejected = store.append_call(msg, NewCall::new("c", json!({})).with_provider_id("c3")).await.unwrap();
    store.resolve_call(rejected, &CallOutcome::Rejected { reason: String::new() }).await.unwrap();

    let cancelled = store.append_call(msg, NewCall::new("d", json!({})).with_provider_id("c4")).await.unwrap();
    store.resolve_call(cancelled, &CallOutcome::Cancelled).await.unwrap();

    // Never resolved: a crash survivor.
    store.append_call(msg, NewCall::new("e", json!({})).with_provider_id("c5")).await.unwrap();
    msg
}

#[tokio::test]
async fn every_call_state_gets_a_result_the_model_can_read() {
    let (store, frame) = store_and_frame("p5").await;
    seed_states(&store, frame).await;

    let msgs = LinearAssembler::new()
        .with_projection(strict())
        .build(&store, &input(frame, SystemContext::base("BASE"), ModelInfo::default()))
        .await
        .unwrap();

    let results: Vec<(&str, &str)> = msgs
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| (m["tool_call_id"].as_str().unwrap(), m["content"].as_str().unwrap()))
        .collect();
    assert_eq!(results, vec![
        ("c1", "ok"),
        ("c2", "Error: boom"),
        // The rejection recorded an empty reason: the configured note stands in.
        ("c3", "User rejected this tool call."),
        // A recorded note wins over the configured default.
        ("c4", "Cancelled by user."),
        ("c5", "Error: tool call was interrupted."),
    ]);

    // The assistant turn itself: calls in order, and a stand-in reasoning
    // because none was recorded.
    let asst = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(asst["tool_calls"][0], json!({
        "id": "c1", "type": "function",
        "function": { "name": "a", "arguments": "{}" },
    }));
    assert_eq!(asst["reasoning_content"], "(no reasoning recorded for this step)");
    assert_eq!(asst["reasoning"], "(no reasoning recorded for this step)");
}

#[tokio::test]
async fn reasoning_echo_is_per_provider_and_never_empty() {
    let (store, frame) = store_and_frame("p6").await;
    store.append(frame, NewMessage::user("q")).await.unwrap();
    store.append(frame, NewMessage::assistant("a", Some("because".into()))).await.unwrap();
    store.append(frame, NewMessage::user("q2")).await.unwrap();
    // An empty stored reasoning must not produce an empty field.
    store.append(frame, NewMessage::assistant("a2", Some(String::new()))).await.unwrap();

    let one = LinearAssembler::new()
        .build(&store, &input(frame, SystemContext::base("B"), ModelInfo::default()))
        .await
        .unwrap();
    let first = one.iter().find(|m| m["content"] == "a").unwrap();
    assert_eq!(first["reasoning_content"], "because");
    assert!(first.get("reasoning").is_none(), "ContentOnly must not echo `reasoning`");
    let second = one.iter().find(|m| m["content"] == "a2").unwrap();
    assert!(second.get("reasoning_content").is_none());

    let both = LinearAssembler::new()
        .with_projection(strict())
        .build(&store, &input(frame, SystemContext::base("B"), ModelInfo::default()))
        .await
        .unwrap();
    let first = both.iter().find(|m| m["content"] == "a").unwrap();
    assert_eq!(first["reasoning"], "because");
    // No placeholder for a plain assistant turn — only tool-calling ones need it.
    let second = both.iter().find(|m| m["content"] == "a2").unwrap();
    assert!(second.get("reasoning_content").is_none());
}

struct Digest;

#[async_trait]
impl ToolResultDigest for Digest {
    async fn condense(&self, name: &str, _args: &Value, result: &str) -> Option<String> {
        Some(format!("[{name}: {} chars]", result.len()))
    }
}

#[tokio::test]
async fn over_long_results_are_condensed_only_for_previous_turns() {
    let (store, frame) = store_and_frame("p7").await;

    // Turn 1 (previous), then turn 2 (current), both with a long result.
    for (user, id) in [("first", "c1"), ("second", "c2")] {
        store.append(frame, NewMessage::user(user)).await.unwrap();
        let msg = store.append(frame, NewMessage::assistant("run", None)).await.unwrap();
        let call = store
            .append_call(msg, NewCall::new("read_file", json!({})).with_provider_id(id))
            .await
            .unwrap();
        store
            .resolve_call(call, &CallOutcome::Completed(ToolOutput::Text("x".repeat(100))))
            .await
            .unwrap();
    }

    let cfg = Projection {
        max_tool_result: Some(ResultLimit { max_chars: 10, previous_turns_only: true }),
        ..Projection::default()
    };
    let msgs = LinearAssembler::new()
        .with_projection(cfg.clone())
        .with_digest(Arc::new(Digest))
        .build(&store, &input(frame, SystemContext::base("B"), ModelInfo::default()))
        .await
        .unwrap();

    let results: Vec<&str> = msgs
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(results[0], "[read_file: 100 chars]", "a previous turn is condensed");
    assert_eq!(results[1].len(), 100, "the current turn keeps its full output");

    // Without a digest the crate truncates on a char boundary.
    let msgs = LinearAssembler::new()
        .with_projection(cfg)
        .build(&store, &input(frame, SystemContext::base("B"), ModelInfo::default()))
        .await
        .unwrap();
    let first = msgs.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(first["content"], "xxxxxxxxxx… [truncated]");
}

// ── dynamic tool loading ─────────────────────────────────────────────────────

/// An assistant turn with two calls, the activation being the SECOND one.
async fn seed_two_calls(store: &Arc<dyn HistoryStore>, frame: FrameId) -> MessageId {
    store.append(frame, NewMessage::user("use gmail")).await.unwrap();
    let anchor = store.append(frame, NewMessage::assistant("activating", None)).await.unwrap();
    let other = store
        .append_call(anchor, NewCall::new("read_file", json!({})).with_provider_id("c1"))
        .await
        .unwrap();
    store.resolve_call(other, &CallOutcome::Completed(ToolOutput::Text("file".into()))).await.unwrap();
    let act = store
        .append_call(anchor, NewCall::new("activate_tools", json!({"groups":["gmail"]})).with_provider_id("c2"))
        .await
        .unwrap();
    store.resolve_call(act, &CallOutcome::Completed(ToolOutput::Text("activated".into()))).await.unwrap();
    anchor
}

#[tokio::test]
async fn deferred_reference_marks_the_activation_result_not_the_first_one() {
    let (store, frame) = store_and_frame("p8").await;
    let anchor = seed_two_calls(&store, frame).await;

    let msgs = LinearAssembler::new()
        .with_projection(strict())
        .with_activation(Arc::new(Stub(vec![Activation {
            anchor,
            defs: vec![tool_def("mcp__gmail__send")],
        }])))
        .build(&store, &input(frame, SystemContext::base("B"), ModelInfo {
            tool_rendering: ToolRendering::DeferredToolReference,
            ..ModelInfo::default()
        }))
        .await
        .unwrap();

    let tools: Vec<&Value> = msgs.iter().filter(|m| m["role"] == "tool").collect();
    assert!(tools[0].get("_tool_references").is_none(), "the read_file result is not the anchor");
    assert_eq!(tools[1]["_tool_references"], json!(["mcp__gmail__send"]));
}

#[tokio::test]
async fn system_tool_block_is_appended_after_the_result_group() {
    let (store, frame) = store_and_frame("p9").await;
    let anchor = seed_two_calls(&store, frame).await;

    let msgs = LinearAssembler::new()
        .with_projection(strict())
        .with_activation(Arc::new(Stub(vec![Activation {
            anchor,
            defs: vec![tool_def("mcp__gmail__send")],
        }])))
        .build(&store, &input(frame, SystemContext::base("B"), ModelInfo {
            tool_rendering: ToolRendering::SystemToolBlock,
            ..ModelInfo::default()
        }))
        .await
        .unwrap();

    let idx = msgs
        .iter()
        .position(|m| m["role"] == "system" && m.get("tools").is_some())
        .expect("no system+tools block");
    assert_eq!(msgs[idx]["tools"][0]["function"]["name"], "mcp__gmail__send");
    assert!(msgs[idx].get("content").is_none(), "the block carries tools, not content");
    assert_eq!(msgs[idx - 1]["role"], "tool", "it comes right after the group");
    assert!(!msgs.iter().any(|m| m.get("_tool_references").is_some()));
}

#[tokio::test]
async fn inline_mode_injects_nothing_at_all() {
    let (store, frame) = store_and_frame("p10").await;
    let anchor = seed_two_calls(&store, frame).await;

    let msgs = LinearAssembler::new()
        .with_projection(strict())
        .with_activation(Arc::new(Stub(vec![Activation {
            anchor,
            defs: vec![tool_def("mcp__gmail__send")],
        }])))
        .build(&store, &input(frame, SystemContext::base("B"), ModelInfo::default()))
        .await
        .unwrap();

    assert!(!msgs.iter().any(|m| m.get("tools").is_some()));
    assert!(!msgs.iter().any(|m| m.get("_tool_references").is_some()));
}

// ── media ────────────────────────────────────────────────────────────────────

struct Png(&'static str);

#[async_trait]
impl MediaBlob for Png {
    fn name(&self) -> &str { self.0 }
    async fn size(&self) -> Option<u64> { Some(72) }
    async fn head(&self) -> Option<Vec<u8>> { Some(b"\x89PNG\r\n\x1a\n........".to_vec()) }
    async fn read_all(&self) -> Option<Vec<u8>> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0xAA; 64]);
        Some(v)
    }
}

/// Every user message has one image; every tool call produces one.
struct Media;

#[async_trait]
impl MediaSource for Media {
    async fn message_media(&self, _msg: &StoredMessage) -> Vec<Arc<dyn MediaBlob>> {
        vec![Arc::new(Png("shot.png"))]
    }
    async fn call_media(&self, _calls: &[StoredCall]) -> Vec<Arc<dyn MediaBlob>> {
        vec![Arc::new(Png("tool.png"))]
    }
    fn skipped_text(&self, _msg: &StoredMessage, skipped: &[usize]) -> Option<String> {
        (!skipped.is_empty()).then(|| format!("\n[files: {}]", skipped.len()))
    }
}

#[tokio::test]
async fn media_is_inlined_for_the_current_turn_and_textual_before_it() {
    let (store, frame) = store_and_frame("p11").await;
    store.append(frame, NewMessage::user("old picture")).await.unwrap();
    store.append(frame, NewMessage::assistant("seen", None)).await.unwrap();
    store.append(frame, NewMessage::user("new picture")).await.unwrap();

    let msgs = LinearAssembler::new()
        .with_media(Arc::new(Media))
        .build(&store, &input(frame, SystemContext::base("B"), ModelInfo {
            capabilities: vec!["vision".into()],
            ..ModelInfo::default()
        }))
        .await
        .unwrap();

    // The previous turn keeps the textual note, no parts.
    assert_eq!(msgs[1], json!({ "role": "user", "content": "old picture\n[files: 1]" }));
    // The current turn inlines the bytes.
    let current = msgs.last().unwrap();
    assert_eq!(current["content"][0], json!({ "type": "text", "text": "new picture" }));
    assert!(
        current["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
}

#[tokio::test]
async fn a_model_without_vision_never_receives_bytes() {
    let (store, frame) = store_and_frame("p12").await;
    store.append(frame, NewMessage::user("picture")).await.unwrap();

    let msgs = LinearAssembler::new()
        .with_media(Arc::new(Media))
        .build(&store, &input(frame, SystemContext::base("B"), ModelInfo::default()))
        .await
        .unwrap();

    assert_eq!(msgs[1], json!({ "role": "user", "content": "picture\n[files: 1]" }));
}

#[tokio::test]
async fn tool_produced_media_rides_a_synthetic_user_message_after_the_group() {
    let (store, frame) = store_and_frame("p13").await;
    store.append(frame, NewMessage::user("read the image")).await.unwrap();
    let msg = store.append(frame, NewMessage::assistant("reading", None)).await.unwrap();
    let call = store
        .append_call(msg, NewCall::new("read_file", json!({"path":"a.png"})).with_provider_id("c1"))
        .await
        .unwrap();
    store.resolve_call(call, &CallOutcome::Completed(ToolOutput::Text("image".into()))).await.unwrap();

    let msgs = LinearAssembler::new()
        .with_media(Arc::new(Media))
        .build(&store, &input(frame, SystemContext::base("B"), ModelInfo {
            capabilities: vec!["vision".into()],
            ..ModelInfo::default()
        }))
        .await
        .unwrap();

    let last = msgs.last().unwrap();
    assert_eq!(last["role"], "user");
    assert_eq!(last["content"][0]["type"], "image_url");
    assert_eq!(msgs[msgs.len() - 2]["role"], "tool", "it follows the result group");
}
