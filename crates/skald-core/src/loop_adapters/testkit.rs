//! Shared scaffolding for the projection tests: a real owner database seeded
//! **through `SqliteHistory`** (the production write path), a real `agents/`
//! directory, a fake MCP provider, and the assembler a Skald turn runs on.
//!
//! One consumer: [`super::projection_snapshots`], the durable oracle — each
//! scenario's expected wire array lives in `snapshots/*.json`. The arrays were
//! frozen while the old `MessageBuilder` was still alive and a parity harness
//! asserted the two produced the same bytes; that harness is gone with the
//! builder, the snapshots outlived it.
//!
//! Everything volatile is neutralized here rather than scrubbed afterwards:
//! the datetime block is disabled, the fixture's prompt carries no
//! `<!-- SKILLS_LIST -->` (so no index is rendered into it), and the fixture's
//! own identifiers never reach the wire.

#![cfg(test)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_loop::context::{AssembleInput, ContextAssembler, SystemContextSource, TurnInfo};
use agent_loop::ids::{ConversationId, FrameId};
use agent_loop::model::ModelInfo;
use agent_loop::store::{CallOutcome, FrameSpec, HistoryStore, NewCall, NewMessage, NewSummary, Role};
use agent_loop::tool::ToolOutput;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use core_api::message_meta::{Attachment, MessageMetadata};
use core_api::user_fs::{SharedFs, UserFs};

use crate::config::DatetimeConfig;
use crate::llm::DtlMode;
use crate::loop_adapters::activation::SkaldActivationSource;
use crate::loop_adapters::history::SqliteHistory;
use crate::loop_adapters::projection_cfg::skald_assembler;
use crate::loop_adapters::selector::tool_rendering_of;
use crate::loop_adapters::system::AgentSystemContext;
use crate::mcp::{McpProvider, McpTool};
use crate::tools::{ToolResult, tool_names as tn};

pub const AGENT_PROMPT: &str = "You are the parity fixture agent.";  // frozen: the snapshots contain it
pub const EXTRA_STATIC:  &str = "FORMAT RULES";
pub const EXTRA_DYNAMIC: &str = "MEMORY BLOCK";
pub const REMINDER:      &str = "REMEMBER THE RULES";
pub const HISTORY_LIMIT:      usize = 100;
pub const TOOL_RESULT_LIMIT:  usize = 40;

// ── fixture plumbing ─────────────────────────────────────────────────────────

pub fn unique(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{tag}-{}-{nanos}", std::process::id())
}

/// The scenarios share one cwd-relative directory (`agents/`, see
/// [`AgentFixture`]), so they run one at a time: a fixture torn down while a
/// sibling is mid-projection would fail it spuriously.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// An `agents/<id>/` directory, since `crate::agents` resolves agents relative
/// to the process cwd and the projection loads the prompt through it. Removed
/// on drop, so a panicking test does not leave it behind.
pub struct AgentFixture {
    pub id: String,
    dir:    PathBuf,
    /// Held for the fixture's lifetime (see [`SERIAL`]). Poisoning is expected:
    /// a failing scenario panics while holding it, and the next may proceed.
    _lock:  std::sync::MutexGuard<'static, ()>,
}

impl AgentFixture {
    pub fn new() -> Self {
        let _lock = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let id = unique("parity-agent");
        let dir = Path::new("agents").join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("AGENT.md"), AGENT_PROMPT).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            json!({
                "name": "Parity fixture",
                "description": "projection parity",
                "type": "task",
            })
            .to_string(),
        )
        .unwrap();
        Self { id, dir, _lock }
    }
}

impl Drop for AgentFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// An owner database with one session and its root frame.
pub struct Db {
    pub pool:  Arc<SqlitePool>,
    pub store: Arc<dyn HistoryStore>,
    pub frame: FrameId,
    path:      PathBuf,
}

impl Db {
    pub async fn new(tag: &str) -> Self {
        let path = std::env::temp_dir().join(format!("{}.db", unique(tag)));
        let pool = Arc::new(crate::db::create_user_pool(&path, None).await.unwrap());
        sqlx::query("INSERT INTO chat_sessions (id, title) VALUES (1, 'parity')")
            .execute(&*pool)
            .await
            .unwrap();
        let store: Arc<dyn HistoryStore> = Arc::new(SqliteHistory::new(pool.clone()));
        let frame = store
            .open_frame(&ConversationId::new("session:1"), None, FrameSpec::root("parity"))
            .await
            .unwrap();
        Self { pool, store, frame, path }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.path.display()));
        }
    }
}

struct FakeMcp {
    tools: Vec<McpTool>,
}

#[async_trait::async_trait]
impl McpProvider for FakeMcp {
    fn tools(&self) -> Vec<McpTool> {
        self.tools.clone()
    }
    fn tools_for(&self, names: &[String]) -> Vec<McpTool> {
        self.tools
            .iter()
            .filter(|t| names.contains(&t.server_name))
            .cloned()
            .collect()
    }
    fn server_descriptions(&self) -> HashMap<String, Option<String>> {
        HashMap::new()
    }
    fn server_infos(&self) -> Vec<Value> {
        Vec::new()
    }
    fn tool_display_name(&self, _server: &str, _tool: &str) -> Option<String> {
        None
    }
    async fn call(&self, _s: &str, _t: &str, _a: Value) -> anyhow::Result<ToolResult> {
        unimplemented!("the projection never calls a tool")
    }
}

pub fn mcp() -> Arc<dyn McpProvider> {
    Arc::new(FakeMcp {
        tools: vec![McpTool {
            server_name:   "gmail".into(),
            name:          "send".into(),
            description:   "send mail".into(),
            input_schema:  json!({ "type": "object" }),
            title:         None,
            output_schema: None,
            annotations:   None,
            task_support:  None,
        }],
    })
}

/// The datetime block is disabled: it embeds `now()`, which no snapshot can
/// pin down.
pub fn datetime() -> DatetimeConfig {
    DatetimeConfig { enabled: false, timezone: None }
}

/// The base tool definitions the projection is handed.
pub fn config_defs() -> Arc<Vec<Value>> {
    Arc::new(vec![json!({
        "type": "function",
        "function": { "name": "config_get", "parameters": { "type": "object" } }
    })])
}

/// What the projection is run with, so a difference can only come from the
/// stored state.
pub struct Case {
    pub dtl:          DtlMode,
    pub cache_hints:  bool,
    pub capabilities: Vec<String>,
    pub fs:           Option<Arc<UserFs>>,
}

impl Default for Case {
    fn default() -> Self {
        Self { dtl: DtlMode::None, cache_hints: false, capabilities: Vec::new(), fs: None }
    }
}

/// Projects the seeded state into the wire messages a model would receive.
pub async fn project(db: &Db, agent: &AgentFixture, case: &Case) -> Vec<Value> {
    let config_defs = config_defs();

    let system_source = AgentSystemContext {
        agent_id:       agent.id.clone(),
        extra_static:   Some(EXTRA_STATIC.to_string()),
        extra_dynamic:  Some(EXTRA_DYNAMIC.to_string()),
        tail_reminder:  Some(REMINDER.to_string()),
        substitutions:  HashMap::new(),
        pool:           db.pool.clone(),
        shared_pool:    db.pool.clone(),
        user_id:        "u1".into(),
        mcp:            mcp(),
        // Skill-less by construction: the fixture's prompt has no sentinel, so
        // nothing here is ever rendered from it.
        fs:             SharedFs::new(UserFs::new(
            "u1",
            PathBuf::from("/wd/homes/u1"),
            "skald-u1",
            PathBuf::from("/root"),
            vec![],
            vec![],
            None,
        )),
        project_root:   None,
        scratchpad_sid: 1,
        datetime:       datetime(),
        // A cache of its own per projection: each case must see a freshly
        // assembled prefix, never one another case left behind.
        sandbox_commands: Arc::new(Vec::new()),
        has_execute_cmd: false,
        prefix_cache:   Arc::new(crate::loop_adapters::prefix_cache::PrefixCache::new()),
    };
    let system = system_source
        .system_context(&TurnInfo {
            conversation: ConversationId::new("session:1"),
            frame:        db.frame,
            agent:        agent.id.clone(),
            user_message: None,
        })
        .await
        .unwrap();

    let assembler = skald_assembler(
        Arc::new(SkaldActivationSource::new(
            db.pool.clone(),
            mcp(),
            config_defs.clone(),
            1,
            None,
        )),
        case.fs.clone(),
        Some(HISTORY_LIMIT),
        // The snapshots exercise the window, so automatic compaction stays off.
        false,
        Some(TOOL_RESULT_LIMIT),
    );
    assembler
        .build(&db.store, &AssembleInput {
            frame:  db.frame,
            system,
            model:  ModelInfo {
                prompt_cache:   case.cache_hints,
                capabilities:   case.capabilities.clone(),
                tool_rendering: tool_rendering_of(case.dtl),
                extras:         Value::Null,
            },
            round: 0,
        })
        .await
        .unwrap()
}

/// Compares message by message, so a failure names the first divergence instead
/// of dumping two arrays.
pub fn assert_same(expected: &[Value], actual: &[Value], label: &str) {
    for (i, (e, a)) in expected.iter().zip(actual.iter()).enumerate() {
        assert_eq!(
            e,
            a,
            "{label}: message {i} diverges\n  expected: {}\n  actual: {}",
            serde_json::to_string_pretty(e).unwrap(),
            serde_json::to_string_pretty(a).unwrap()
        );
    }
    assert_eq!(
        expected.len(),
        actual.len(),
        "{label}: message COUNT diverges ({} expected vs {} actual); first extra: {:?}",
        expected.len(),
        actual.len(),
        expected
            .get(actual.len().min(expected.len()))
            .or_else(|| actual.get(expected.len().min(actual.len()))),
    );
}

// ── snapshots ────────────────────────────────────────────────────────────────

/// Set to `1` to rewrite the stored arrays from the current projection. Review
/// the diff: a snapshot changing means the bytes a model receives changed.
pub const UPDATE_ENV: &str = "UPDATE_PROJECTION_SNAPSHOTS";

pub fn snapshot_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/loop_adapters/snapshots")
        .join(format!("{name}.json"))
}

/// Asserts `actual` against the stored array, or rewrites it under [`UPDATE_ENV`].
pub fn assert_snapshot(name: &str, actual: &[Value]) {
    let path = snapshot_path(name);
    if std::env::var(UPDATE_ENV).as_deref() == Ok("1") {
        write_snapshot(name, actual);
        return;
    }
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing snapshot {}: {e}\nrun with {UPDATE_ENV}=1 to create it",
            path.display()
        )
    });
    let expected: Vec<Value> = serde_json::from_str(&raw).unwrap();
    assert_same(&expected, actual, name);
}

/// Writes the stored array (pretty, newline-terminated: it is reviewed as a diff).
pub fn write_snapshot(name: &str, value: &[Value]) {
    let path = snapshot_path(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut json = serde_json::to_string_pretty(value).unwrap();
    json.push('\n');
    std::fs::write(&path, json).unwrap();
}

// ── scenarios: the seeded state ──────────────────────────────────────────────
//
// One function per scenario: the state, separate from what is asserted about it.

/// A plain exchange, including the two consecutive user rows that exercise the
/// coalescing rule.
pub async fn seed_plain(db: &Db) {
    db.store.append(db.frame, NewMessage::user("hello")).await.unwrap();
    db.store
        .append(db.frame, NewMessage::assistant("hi there", Some("thinking".into())))
        .await
        .unwrap();
    db.store.append(db.frame, NewMessage::user("one")).await.unwrap();
    db.store.append(db.frame, NewMessage::user("two")).await.unwrap();
}

pub async fn seed_scratchpad(db: &Db) {
    crate::db::scratchpad::upsert(&db.pool, 1, "plan", "step one").await.unwrap();
    db.store.append(db.frame, NewMessage::user("go")).await.unwrap();
}

/// One assistant turn with a call in each terminal state.
pub async fn seed_tool_round(db: &Db) {
    db.store.append(db.frame, NewMessage::user("work")).await.unwrap();
    let msg = db.store.append(db.frame, NewMessage::assistant("calling", None)).await.unwrap();

    let done = db
        .store
        .append_call(msg, NewCall::new("read_file", json!({ "path": "a.md" })))
        .await
        .unwrap();
    db.store
        .resolve_call(done, &CallOutcome::Completed(ToolOutput::Text("content".into())))
        .await
        .unwrap();

    let failed = db.store.append_call(msg, NewCall::new("write_file", json!({}))).await.unwrap();
    db.store.resolve_call(failed, &CallOutcome::Failed("disk full".into())).await.unwrap();

    let rejected = db.store.append_call(msg, NewCall::new("execute_cmd", json!({}))).await.unwrap();
    db.store
        .resolve_call(rejected, &CallOutcome::Rejected { reason: "no".into() })
        .await
        .unwrap();

    let cancelled = db.store.append_call(msg, NewCall::new("glob", json!({}))).await.unwrap();
    db.store.resolve_call(cancelled, &CallOutcome::Cancelled).await.unwrap();
}

/// A call left `running`, exactly as a crash leaves it.
pub async fn seed_interrupted(db: &Db) {
    db.store.append(db.frame, NewMessage::user("run it")).await.unwrap();
    let msg = db.store.append(db.frame, NewMessage::assistant("running", None)).await.unwrap();
    db.store
        .append_call(msg, NewCall::new("execute_cmd", json!({ "command": "sleep 100" })))
        .await
        .unwrap();
}

/// Two turns with an over-limit result each: only the first is condensed.
pub async fn seed_condensed(db: &Db) {
    for (q, path) in [("first", "big.txt"), ("second", "other.txt")] {
        db.store.append(db.frame, NewMessage::user(q)).await.unwrap();
        let msg = db.store.append(db.frame, NewMessage::assistant("reading", None)).await.unwrap();
        let call = db
            .store
            .append_call(msg, NewCall::new("read_file", json!({ "path": path })))
            .await
            .unwrap();
        db.store
            .resolve_call(
                call,
                &CallOutcome::Completed(ToolOutput::Text("x".repeat(TOOL_RESULT_LIMIT * 3))),
            )
            .await
            .unwrap();
    }
}

pub async fn seed_summary(db: &Db) {
    let m1 = db.store.append(db.frame, NewMessage::user("ancient")).await.unwrap();
    db.store.append(db.frame, NewMessage::assistant("old reply", None)).await.unwrap();
    db.store.append(db.frame, NewMessage::user("recent")).await.unwrap();
    db.store
        .save_summary(db.frame, NewSummary {
            text:          "Earlier they discussed ancient things.".into(),
            covered_up_to: m1,
        })
        .await
        .unwrap();
}

/// An activation round: an unrelated call first, so the DTL marker has a wrong
/// place to land if the anchor rule regresses.
pub async fn seed_activation(db: &Db) {
    db.store.append(db.frame, NewMessage::user("use gmail")).await.unwrap();
    let anchor = db
        .store
        .append(db.frame, NewMessage::assistant("activating", None))
        .await
        .unwrap();
    let other = db.store.append_call(anchor, NewCall::new("read_file", json!({}))).await.unwrap();
    db.store
        .resolve_call(other, &CallOutcome::Completed(ToolOutput::Text("f".into())))
        .await
        .unwrap();
    let act = db
        .store
        .append_call(anchor, NewCall::new(tn::ACTIVATE_TOOLS, json!({ "groups": ["gmail"] })))
        .await
        .unwrap();
    db.store
        .resolve_call(act, &CallOutcome::Completed(ToolOutput::Text("activated".into())))
        .await
        .unwrap();
    crate::db::activated_tools::grant(&db.pool, 1, None, anchor.get(), "mcp", "gmail")
        .await
        .unwrap();
}

/// A real PNG under the caller's uploads dir, plus the `UserFs` that authorizes
/// it. Removed on drop.
pub struct MediaHome {
    root:   PathBuf,
    pub fs: Arc<UserFs>,
}

impl MediaHome {
    pub fn new() -> Self {
        let root = std::env::temp_dir().join(unique("parity-home"));
        let uploads = root.join("uploads/1");
        std::fs::create_dir_all(&uploads).unwrap();
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0xAA; 64]);
        std::fs::write(uploads.join("shot.png"), png).unwrap();
        let fs = Arc::new(UserFs::new(
            "u1",
            root.clone(),
            "skald-u1",
            PathBuf::from("/root"),
            vec![],
            vec![],
            None,
        ));
        Self { root, fs }
    }
}

impl Drop for MediaHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The same attachment on an older turn (textual path) and on the current one
/// (inlined when the model can see it).
pub async fn seed_media(db: &Db) {
    let meta = MessageMetadata {
        attachments: vec![Attachment {
            path:     "uploads/1/shot.png".into(),
            name:     "shot.png".into(),
            mimetype: Some("image/png".into()),
            filesize: None,
        }],
        ..Default::default()
    };
    let with_attachment = |content: &str| NewMessage {
        role:      Role::User,
        content:   content.to_string(),
        synthetic: false,
        reasoning: None,
        metadata:  Some(serde_json::to_value(&meta).unwrap()),
    };

    db.store.append(db.frame, with_attachment("old shot")).await.unwrap();
    db.store.append(db.frame, NewMessage::assistant("seen", None)).await.unwrap();
    db.store.append(db.frame, with_attachment("new shot")).await.unwrap();
}
