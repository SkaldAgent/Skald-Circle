//! End-to-end test of the block-and-poll Tasks client against a real stdio MCP
//! subprocess.
//!
//! A tiny Python "server" exposes one tool with `execution.taskSupport: "required"`.
//! On `tools/call` it returns a `CreateTaskResult` (a durable handle, no `content`)
//! instead of a result; the client then polls `tasks/get` until `completed` and
//! fetches the real answer via `tasks/result`. A second mode never completes, so the
//! test can drop the call mid-poll and assert the client sends `tasks/cancel`.
//! Skipped if `python3` is absent.

use std::io::Write;
use std::process::Command;
use std::time::Duration;

use mcp_client::config::{McpServerConfig, McpTransport};
use mcp_client::server::McpServer;
use mcp_client::McpCallResult;

/// argv[1] = mode ("complete" | "cancel"); argv[2] = marker file (cancel mode).
const FAKE_SERVER: &str = r#"
import sys, json

mode   = sys.argv[1] if len(sys.argv) > 1 else "complete"
marker = sys.argv[2] if len(sys.argv) > 2 else None
get_count = 0

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def readline():
    line = sys.stdin.readline()
    if not line:
        return None
    line = line.strip()
    return line if line else readline()

while True:
    raw = readline()
    if raw is None:
        break
    msg = json.loads(raw)
    mid = msg.get("id")
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2025-11-25", "capabilities": {},
            "serverInfo": {"name": "fake", "version": "0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [
            {"name": "gen", "description": "deferred generator",
             "inputSchema": {"type": "object", "properties": {}},
             "execution": {"taskSupport": "required"}}]}})
    elif method == "tools/call":
        # Defer: return a task handle (no `content`).
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "taskId": "job-1", "status": "working", "pollInterval": 500}})
    elif method == "tasks/get":
        get_count += 1
        if mode == "complete" and get_count >= 2:
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "taskId": "job-1", "status": "completed"}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "taskId": "job-1", "status": "working", "pollInterval": 500}})
    elif method == "tasks/result":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "content": [{"type": "text", "text": "the real answer"}]}})
    elif method == "tasks/cancel":
        if marker:
            with open(marker, "w") as f:
                f.write((msg.get("params") or {}).get("taskId", ""))
        if mid is not None:
            send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown"}})
"#;

fn python3_available() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

fn write_script() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("skald_tasks_{}.py", std::process::id()));
    std::fs::File::create(&path).unwrap().write_all(FAKE_SERVER.as_bytes()).unwrap();
    path
}

fn cfg(script: &std::path::Path, mode: &str, marker: Option<&std::path::Path>) -> McpServerConfig {
    let mut args = vec![script.to_string_lossy().to_string(), mode.to_string()];
    if let Some(m) = marker {
        args.push(m.to_string_lossy().to_string());
    }
    McpServerConfig {
        name: "fake".to_string(),
        transport: McpTransport::Stdio,
        command: Some("python3".to_string()),
        args: Some(args),
        env: None,
        url: None,
        api_key: None,
        launch_in: None,
    }
}

#[tokio::test]
async fn task_is_polled_to_completion_and_returns_real_result() {
    if !python3_available() {
        eprintln!("python3 not found — skipping tasks integration test");
        return;
    }
    let script = write_script();
    let server = McpServer::start(&cfg(&script, "complete", None), None, None, None)
        .await
        .expect("server should start");

    // The tool opts into tasks (taskSupport: required); call_tool must add the
    // `task` field, poll to completion, and return the real result — not the handle.
    let result = server.call_tool("gen", serde_json::json!({}))
        .await
        .expect("task should complete");
    match result {
        McpCallResult::Text(t) => assert_eq!(t, "the real answer"),
        other => panic!("expected the polled Text result, got {other:?}"),
    }

    let _ = std::fs::remove_file(&script);
}

#[tokio::test]
async fn dropping_a_polling_call_sends_tasks_cancel() {
    if !python3_available() {
        eprintln!("python3 not found — skipping tasks cancel integration test");
        return;
    }
    let script = write_script();
    let marker = std::env::temp_dir().join(format!("skald_tasks_marker_{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);

    let server = McpServer::start(&cfg(&script, "cancel", Some(&marker)), None, None, None)
        .await
        .expect("server should start");

    // In "cancel" mode tasks/get never completes, so the call polls forever; time out
    // to drop the future mid-poll, which must fire tasks/cancel via the drop-guard.
    let timed = tokio::time::timeout(
        Duration::from_millis(900),
        server.call_tool("gen", serde_json::json!({})),
    ).await;
    assert!(timed.is_err(), "call should still be polling (timed out), not resolved");

    // Give the guard's spawned tasks/cancel time to reach the server.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let recorded = std::fs::read_to_string(&marker).unwrap_or_default();
    assert_eq!(recorded, "job-1", "server should have received tasks/cancel for job-1");

    let _ = std::fs::remove_file(&script);
    let _ = std::fs::remove_file(&marker);
}
