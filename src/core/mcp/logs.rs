//! Per-server MCP log files.
//!
//! Consumes [`McpLogLine`]s emitted by the MCP client crate and appends each to a
//! dedicated file `logs/mcp/<name>.log`. Sources captured (see `crates/mcp-client`):
//! child `stderr` (stdio), diverted `notifications/message` log records, and
//! connection lifecycle events. No SQLite — a plain file per server, meant to be
//! scanned later (e.g. by a diagnostics agent) for `[error]`/`[warning]` lines.

use std::collections::HashMap;
use std::path::PathBuf;

use mcp_client::McpLogLine;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Size at which a server's `.log` is rotated to `.log.1` (one backup kept), so a
/// chatty server can't grow its file without bound.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Background task: drain `rx` and append every line to its server's file until
/// shutdown or the channel closes. Spawned once from `McpManager::new`.
pub(super) async fn log_consumer(
    mut rx:   mpsc::UnboundedReceiver<McpLogLine>,
    shutdown: CancellationToken,
) {
    let mut writer = LogWriter::new(PathBuf::from("logs").join("mcp"));
    if let Err(e) = tokio::fs::create_dir_all(&writer.dir).await {
        warn!("mcp logs: cannot create {}: {e}", writer.dir.display());
        return;
    }
    info!("mcp: per-server log consumer started ({})", writer.dir.display());

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("mcp: log consumer shutdown");
                break;
            }
            msg = rx.recv() => match msg {
                Some(line) => writer.write_line(line).await,
                None       => break,
            }
        }
    }
}

/// Holds one append handle per server (plus its tracked byte size for rotation).
struct LogWriter {
    dir:   PathBuf,
    files: HashMap<String, (File, u64)>,
}

impl LogWriter {
    fn new(dir: PathBuf) -> Self {
        Self { dir, files: HashMap::new() }
    }

    /// `logs/mcp/<sanitized>.log`. The name is sanitized so a server called
    /// `foo/bar` or `a b` can't escape the directory or produce an odd filename.
    fn path_for(&self, server: &str) -> PathBuf {
        self.dir.join(format!("{}.log", sanitize(server)))
    }

    async fn open(&self, server: &str) -> std::io::Result<(File, u64)> {
        let path = self.path_for(server);
        let file = OpenOptions::new().create(true).append(true).open(&path).await?;
        // Seed the tracked size from the existing file so appends keep counting
        // toward the rotation threshold across restarts.
        let size = file.metadata().await.map(|m| m.len()).unwrap_or(0);
        Ok((file, size))
    }

    /// Renames `<name>.log` → `<name>.log.1` (overwriting any previous backup) and
    /// reopens a fresh, empty handle.
    async fn rotate(&self, server: &str) -> std::io::Result<(File, u64)> {
        let path   = self.path_for(server);
        let backup = PathBuf::from(format!("{}.1", path.display()));
        let _ = tokio::fs::rename(&path, &backup).await; // best-effort
        self.open(server).await
    }

    async fn write_line(&mut self, line: McpLogLine) {
        let record = format_record(&line);
        let bytes  = record.len() as u64;

        // Open on first use for this server.
        if !self.files.contains_key(&line.server) {
            match self.open(&line.server).await {
                Ok(handle) => { self.files.insert(line.server.clone(), handle); }
                Err(e)     => { warn!("mcp logs: open failed for '{}': {e}", line.server); return; }
            }
        }

        // Rotate before writing if this line would push the file over the cap.
        let over_cap = self.files.get(&line.server)
            .map(|(_, sz)| *sz + bytes > MAX_LOG_BYTES)
            .unwrap_or(false);
        if over_cap {
            match self.rotate(&line.server).await {
                Ok(handle) => { self.files.insert(line.server.clone(), handle); }
                Err(e)     => { warn!("mcp logs: rotate failed for '{}': {e}", line.server); }
            }
        }

        if let Some((file, size)) = self.files.get_mut(&line.server) {
            match file.write_all(record.as_bytes()).await {
                Ok(()) => {
                    *size += bytes;
                    let _ = file.flush().await;
                }
                Err(e) => {
                    warn!("mcp logs: write failed for '{}': {e}", line.server);
                    // Drop the handle so the next line retries a fresh open.
                    self.files.remove(&line.server);
                }
            }
        }
    }
}

/// `2026-07-03T12:34:56.789Z [warning]   <text>` — an ISO-8601 UTC timestamp, the
/// padded level tag, then the text. The padded tag keeps files column-aligned and
/// makes `[error]`/`[warning]` trivial to grep.
fn format_record(line: &McpLogLine) -> String {
    let ts  = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let tag = format!("[{}]", line.level);
    format!("{ts} {tag:<12} {}\n", line.text)
}

/// Keeps ASCII alphanumerics and `-_.`; everything else becomes `_`. Guarantees a
/// non-empty, path-separator-free filename component.
fn sanitize(name: &str) -> String {
    let s: String = name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    if s.is_empty() { "unknown".to_string() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_separators_and_spaces() {
        assert_eq!(sanitize("gmail"), "gmail");
        assert_eq!(sanitize("foo/bar"), "foo_bar");
        assert_eq!(sanitize("a b"), "a_b");
        assert_eq!(sanitize("claude_ai_Gmail"), "claude_ai_Gmail");
        assert_eq!(sanitize(""), "unknown");
    }

    #[test]
    fn format_record_has_timestamp_tag_and_text() {
        let rec = format_record(&McpLogLine::stderr("srv", "hello"));
        assert!(rec.contains("[stderr]"));
        assert!(rec.ends_with("hello\n"));
        assert!(rec.starts_with("20")); // year prefix of the ISO timestamp
    }

    #[tokio::test]
    async fn write_line_appends_one_file_per_server() {
        let dir = std::env::temp_dir().join(format!("skald_mcplogs_{}", std::process::id()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await.unwrap();

        let mut writer = LogWriter::new(dir.clone());
        writer.write_line(McpLogLine::stderr("gmail", "banner line")).await;
        writer.write_line(McpLogLine::lifecycle("gmail", "connected — 3 tool(s)")).await;
        writer.write_line(McpLogLine::from_message(
            "firecrawl",
            &serde_json::json!({ "level": "error", "data": "boom" }),
        )).await;

        // One file per server, named from the sanitized server name.
        let gmail = tokio::fs::read_to_string(dir.join("gmail.log")).await.unwrap();
        assert!(gmail.contains("[stderr]") && gmail.contains("banner line"));
        assert!(gmail.contains("[lifecycle]") && gmail.contains("connected — 3 tool(s)"));

        let fire = tokio::fs::read_to_string(dir.join("firecrawl.log")).await.unwrap();
        assert!(fire.contains("[error]") && fire.contains("boom"));
        // gmail's lines must not leak into firecrawl's file.
        assert!(!fire.contains("banner line"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
