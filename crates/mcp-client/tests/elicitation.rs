//! End-to-end test of the elicitation path against a real stdio MCP subprocess.
//!
//! A tiny Python "server" exposes one tool that, when called, issues a
//! server→client `elicitation/create` and echoes back whatever value it receives.
//! This exercises the read-loop request branch, the spawned reply writer, the
//! capability handshake, and `content` passthrough. Skipped if `python3` is absent.

use std::io::Write;
use std::process::Command;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use mcp_client::config::{McpServerConfig, McpTransport};
use mcp_client::server::{
    ElicitationAction, ElicitationHandler, ElicitationReply, ElicitationRequest, McpServer,
};
use mcp_client::McpCallResult;

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
            "protocolVersion": "2025-06-18", "capabilities": {},
            "serverInfo": {"name": "fake", "version": "0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": [
            {"name": "need_secret", "description": "asks for a secret",
             "inputSchema": {"type": "object", "properties": {}}}]}})
    elif method == "tools/call":
        eid = "elicit-1"
        send({"jsonrpc": "2.0", "id": eid, "method": "elicitation/create", "params": {
            "message": "Enter password",
            "requestedSchema": {"type": "object", "properties": {
                "password": {"type": "string", "format": "password"}}}}})
        reply = None
        while reply is None:
            r = readline()
            if r is None:
                break
            rr = json.loads(r)
            if rr.get("id") == eid:
                reply = rr
        val = (reply or {}).get("result", {}).get("content", {}).get("password", "<none>")
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "content": [{"type": "text", "text": "got:" + val}], "isError": False}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown"}})
"#;

struct AcceptHandler;

#[async_trait]
impl ElicitationHandler for AcceptHandler {
    async fn handle(&self, _server: &str, req: ElicitationRequest) -> ElicitationReply {
        assert_eq!(req.message, "Enter password");
        assert!(req.requested_schema.get("properties").is_some());
        ElicitationReply {
            action:  ElicitationAction::Accept,
            content: Some(json!({ "password": "hunter2" })),
        }
    }
}

fn python3_available() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

#[tokio::test]
async fn elicitation_roundtrip_returns_secret_to_server() {
    if !python3_available() {
        eprintln!("python3 not found — skipping elicitation integration test");
        return;
    }

    let script_path = std::env::temp_dir().join(format!("skald_elicit_{}.py", std::process::id()));
    std::fs::File::create(&script_path)
        .unwrap()
        .write_all(FAKE_SERVER.as_bytes())
        .unwrap();

    let cfg = McpServerConfig {
        name:      "fake".to_string(),
        transport: McpTransport::Stdio,
        command:   Some("python3".to_string()),
        args:      Some(vec![script_path.to_string_lossy().to_string()]),
        env:       None,
        url:       None,
        api_key:   None,
    };

    let server = McpServer::start(&cfg, None, None, Some(Arc::new(AcceptHandler)))
        .await
        .expect("server should start");

    let result = server
        .call_tool("need_secret", json!({}))
        .await
        .expect("tool call should succeed");

    // The fake server returns text content (no structuredContent) → Text variant.
    match result {
        McpCallResult::Text(s) => assert_eq!(s, "got:hunter2"),
        other => panic!("expected Text, got {other:?}"),
    }

    let _ = std::fs::remove_file(&script_path);
}
