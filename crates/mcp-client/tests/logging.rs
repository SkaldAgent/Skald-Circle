//! End-to-end test of per-server diagnostic capture against a real stdio MCP
//! subprocess.
//!
//! A tiny Python "server" prints a banner to **stderr** and, right after
//! `notifications/initialized`, emits both a `notifications/message` log record and
//! a business `event/ping` notification to stdout. The test asserts that:
//!   - the stderr banner arrives on `log_tx` tagged `stderr`,
//!   - the `notifications/message` arrives on `log_tx` with its MCP level, and is
//!     **not** delivered to `notification_tx` (it's diverted away from TIC),
//!   - the business `event/ping` still arrives on `notification_tx`.
//! Skipped if `python3` is absent.

use std::io::Write;
use std::process::Command;
use std::time::Duration;

use mcp_client::config::{McpServerConfig, McpTransport};
use mcp_client::server::{McpNotification, McpServer};
use mcp_client::McpLogLine;
use serde_json::Value;
use tokio::sync::mpsc;

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
        # A diagnostic banner on stderr (the primary, future-proof log source).
        print("startup banner on stderr", file=sys.stderr, flush=True)
        # An MCP logging record (should be diverted to the log file, NOT TIC).
        send({"jsonrpc": "2.0", "method": "notifications/message",
              "params": {"level": "warning", "logger": "test", "data": "disk almost full"}})
        # A business event (should still reach the notification queue / TIC).
        send({"jsonrpc": "2.0", "method": "event/ping", "params": {"n": 1}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": []}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown"}})
"#;

fn python3_available() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

fn write_script() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("skald_logging_{}.py", std::process::id()));
    std::fs::File::create(&path).unwrap().write_all(FAKE_SERVER.as_bytes()).unwrap();
    path
}

/// Drains a channel for up to `budget`, returning everything received.
async fn drain<T>(rx: &mut mpsc::UnboundedReceiver<T>, budget: Duration) -> Vec<T> {
    let mut out = Vec::new();
    let deadline = tokio::time::Instant::now() + budget;
    while let Ok(Some(item)) = tokio::time::timeout_at(deadline, rx.recv()).await {
        out.push(item);
    }
    out
}

#[tokio::test]
async fn stderr_and_log_records_are_captured_and_diverted() {
    if !python3_available() {
        eprintln!("python3 not found — skipping per-server logging integration test");
        return;
    }
    let script = write_script();
    let cfg = McpServerConfig {
        name: "fake".to_string(),
        transport: McpTransport::Stdio,
        command: Some("python3".to_string()),
        args: Some(vec![script.to_string_lossy().to_string()]),
        env: None,
        url: None,
        api_key: None,
        launch_in: None,
    };

    let (notif_tx, mut notif_rx) = mpsc::unbounded_channel::<McpNotification>();
    let (log_tx, mut log_rx)     = mpsc::unbounded_channel::<McpLogLine>();

    let _server = McpServer::start(&cfg, Some(notif_tx), Some(log_tx), None)
        .await
        .expect("server should start");

    let logs: Vec<McpLogLine>   = drain(&mut log_rx,   Duration::from_millis(1500)).await;
    let notes: Vec<McpNotification> = drain(&mut notif_rx, Duration::from_millis(500)).await;

    // stderr banner captured on the log channel.
    assert!(
        logs.iter().any(|l| l.level == "stderr" && l.text.contains("startup banner on stderr")),
        "expected a [stderr] log line, got: {logs:?}",
    );
    // notifications/message captured with its MCP level + logger prefix.
    assert!(
        logs.iter().any(|l| l.level == "warning" && l.text.contains("disk almost full") && l.text.contains("test")),
        "expected a [warning] log line from notifications/message, got: {logs:?}",
    );
    // The log record was diverted away from the notification queue…
    assert!(
        !notes.iter().any(|(_, msg)| msg.get("method").and_then(Value::as_str) == Some("notifications/message")),
        "notifications/message must NOT reach the notification queue, got: {notes:?}",
    );
    // …while the business event still flowed through it.
    assert!(
        notes.iter().any(|(_, msg)| msg.get("method").and_then(Value::as_str) == Some("event/ping")),
        "expected event/ping on the notification queue, got: {notes:?}",
    );

    let _ = std::fs::remove_file(&script);
}
