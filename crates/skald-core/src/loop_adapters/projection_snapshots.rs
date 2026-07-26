//! The projection's regression net **in the context of Skald**: a real owner
//! database, a real `UserFs`, real DTL rendering — asserted against the wire
//! arrays stored under `snapshots/`.
//!
//! The stored arrays were **frozen while the old `MessageBuilder` still ran
//! beside the new projection and a parity harness asserted they matched**, so
//! each one is a byte-for-byte record of what Skald sent before the projection
//! moved into the library. The harness died with the builder; the record is
//! what survives it.
//!
//! A failure here means the bytes a model receives changed. That is either a
//! bug or a deliberate change; if deliberate, rerun with
//! `UPDATE_PROJECTION_SNAPSHOTS=1` and **review the diff**.
//!
//! The state seeded per scenario lives in [`super::testkit`].

#![cfg(test)]

use serde_json::{Value, json};

use crate::llm::DtlMode;
use crate::loop_adapters::testkit::{
    self, AgentFixture, Case, Db, MediaHome, TOOL_RESULT_LIMIT, assert_snapshot, project,
};

#[tokio::test]
async fn snapshot_plain_conversation() {
    let agent = AgentFixture::new();
    let db = Db::new("snap-plain").await;
    testkit::seed_plain(&db).await;

    let wire = project(&db, &agent, &Case::default()).await;
    assert_snapshot("plain_conversation", &wire);
    // Sanity: the fixture really produced the layers the snapshot means to pin.
    assert!(wire.len() >= 5, "{wire:#?}");
}

#[tokio::test]
async fn snapshot_scratchpad_and_cache_hints() {
    let agent = AgentFixture::new();
    let db = Db::new("snap-scratch").await;
    testkit::seed_scratchpad(&db).await;

    let wire = project(&db, &agent, &Case { cache_hints: true, ..Case::default() }).await;
    assert_snapshot("scratchpad_and_cache_hints", &wire);
    assert!(
        wire[0]["content"][0]["cache_control"].is_object(),
        "the cache breakpoint must be on the static prefix: {:#?}",
        wire[0]
    );
    assert!(wire[1]["content"].as_str().unwrap().contains("<scratchpad>"));
}

#[tokio::test]
async fn snapshot_tool_round_every_state() {
    let agent = AgentFixture::new();
    let db = Db::new("snap-tools").await;
    testkit::seed_tool_round(&db).await;

    let wire = project(&db, &agent, &Case::default()).await;
    assert_snapshot("tool_round_every_state", &wire);
}

#[tokio::test]
async fn snapshot_interrupted_call_survives_a_restart() {
    let agent = AgentFixture::new();
    let db = Db::new("snap-interrupted").await;
    testkit::seed_interrupted(&db).await;

    let wire = project(&db, &agent, &Case::default()).await;
    assert_snapshot("interrupted_call", &wire);
    let tool_msg = wire.iter().find(|m| m["role"] == "tool").unwrap();
    assert!(tool_msg["content"].as_str().unwrap().contains("interrupted"));
}

#[tokio::test]
async fn snapshot_condensed_previous_turn_results() {
    let agent = AgentFixture::new();
    let db = Db::new("snap-condense").await;
    testkit::seed_condensed(&db).await;

    let wire = project(&db, &agent, &Case::default()).await;
    assert_snapshot("condensed_previous_turn", &wire);
    let results: Vec<&str> = wire
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(results[0], "[read_file] read big.txt (120 chars)");
    assert_eq!(results[1].len(), TOOL_RESULT_LIMIT * 3, "the current turn keeps its output");
}

#[tokio::test]
async fn snapshot_with_a_compaction_summary() {
    let agent = AgentFixture::new();
    let db = Db::new("snap-summary").await;
    testkit::seed_summary(&db).await;

    let wire = project(&db, &agent, &Case::default()).await;
    assert_snapshot("compaction_summary", &wire);
    assert!(
        wire.iter().any(|m| {
            m["content"]
                .as_str()
                .is_some_and(|c| c.contains(crate::compactor::SUMMARY_PREFIX))
        }),
        "the summary block must carry Skald's own prefix: {wire:#?}"
    );
}

#[tokio::test]
async fn snapshot_dtl_all_three_modes() {
    let agent = AgentFixture::new();
    let db = Db::new("snap-dtl").await;
    testkit::seed_activation(&db).await;

    for (dtl, name) in [
        (DtlMode::None, "dtl_none"),
        (DtlMode::AnthropicToolReference, "dtl_anthropic_tool_reference"),
        (DtlMode::KimiSystemTools, "dtl_kimi_system_tools"),
    ] {
        let wire = project(&db, &agent, &Case { dtl, ..Case::default() }).await;
        assert_snapshot(name, &wire);

        // The marker rides the activation's own result, not whichever tool
        // result happens to come first in the round.
        if dtl == DtlMode::AnthropicToolReference {
            let tools: Vec<&Value> = wire.iter().filter(|m| m["role"] == "tool").collect();
            assert!(tools[0].get("_tool_references").is_none());
            assert_eq!(tools[1]["_tool_references"], json!(["mcp__gmail__send"]));
        }
    }
}

#[tokio::test]
async fn snapshot_inlined_attachment() {
    let agent = AgentFixture::new();
    let db = Db::new("snap-media").await;
    let home = MediaHome::new();
    testkit::seed_media(&db).await;

    let wire = project(&db, &agent, &Case {
        capabilities: vec!["vision".into()],
        fs:           Some(home.fs.clone()),
        ..Case::default()
    })
    .await;
    assert_snapshot("inlined_attachment", &wire);

    let current = wire.iter().rev().find(|m| m["role"] == "user").unwrap();
    assert_eq!(current["content"][1]["type"], "image_url");
}
