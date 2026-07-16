//! End-to-end test of `tools/list` cursor pagination against a real stdio MCP
//! subprocess.
//!
//! A tiny Python "server" returns its tools across two pages: the first
//! `tools/list` (no `cursor`) yields one tool plus a `nextCursor`; the follow-up
//! (with that `cursor`) yields the rest and no `nextCursor`. This exercises the
//! cursor loop in `McpServer::start`. Skipped if `python3` is absent.

use std::io::Write;
use std::process::Command;

use mcp_client::config::{McpServerConfig, McpTransport};
use mcp_client::server::McpServer;

const FAKE_SERVER: &str = r#"
import sys, json

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
        cursor = (msg.get("params") or {}).get("cursor")
        if cursor is None:
            # Page 1: one tool + a cursor pointing at the next page.
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "tools": [{"name": "alpha", "description": "first",
                           "inputSchema": {"type": "object", "properties": {}}}],
                "nextCursor": "page-2"}})
        elif cursor == "page-2":
            # Page 2: the rest, no further cursor → loop terminates.
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "tools": [{"name": "beta",  "description": "second",
                           "inputSchema": {"type": "object", "properties": {}}},
                          {"name": "gamma", "description": "third",
                           "inputSchema": {"type": "object", "properties": {}}}]}})
        else:
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": []}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown"}})
"#;

fn python3_available() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

#[tokio::test]
async fn tools_list_follows_next_cursor_across_pages() {
    if !python3_available() {
        eprintln!("python3 not found — skipping pagination integration test");
        return;
    }

    let script_path =
        std::env::temp_dir().join(format!("skald_paginate_{}.py", std::process::id()));
    std::fs::File::create(&script_path)
        .unwrap()
        .write_all(FAKE_SERVER.as_bytes())
        .unwrap();

    let cfg = McpServerConfig {
        name: "fake".to_string(),
        transport: McpTransport::Stdio,
        command: Some("python3".to_string()),
        args: Some(vec![script_path.to_string_lossy().to_string()]),
        env: None,
        url: None,
        api_key: None,
        launch_in: None,
    };

    let server = McpServer::start(&cfg, None, None, None)
        .await
        .expect("server should start");

    let names: Vec<&str> = server.tools().iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["alpha", "beta", "gamma"],
        "all pages should be collected"
    );

    let _ = std::fs::remove_file(&script_path);
}
