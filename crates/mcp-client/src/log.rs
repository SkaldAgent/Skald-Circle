//! Per-server diagnostic log lines.
//!
//! An MCP server produces diagnostics from three places: its process `stderr`
//! (stdio transport), MCP `notifications/message` log records, and connection
//! lifecycle events (start failure / disconnect). This crate stays a generic
//! transport — it *emits* [`McpLogLine`]s over a channel but never touches the
//! disk. The host (`McpManager`) owns the channel and writes each line to a
//! per-server file `logs/mcp/<name>.log`.
//!
//! Note: the MCP `logging` utility (`notifications/message` + `logging/setLevel`)
//! is deprecated from the 2026-07-28 draft (SEP-2577) in favour of `stderr`, so
//! `stderr` is the primary, future-proof source; `notifications/message` is
//! captured for interop with servers that still emit it.

use std::borrow::Cow;

use serde_json::Value;
use tokio::sync::mpsc;

/// A single diagnostic line from an MCP server, routed by the host to that
/// server's log file.
#[derive(Debug, Clone)]
pub struct McpLogLine {
    /// The MCP server that produced the line (selects the target file).
    pub server: String,
    /// Severity/kind tag, written inside `[...]`: `"stderr"` for raw child
    /// stderr, an MCP log level (`"debug"`..`"emergency"`) for
    /// `notifications/message`, or `"lifecycle"` for start/disconnect events.
    pub level: Cow<'static, str>,
    /// Human-readable text, already flattened to a single line (no trailing `\n`).
    pub text: String,
}

/// Channel the host installs to receive [`McpLogLine`]s from a running server.
pub type McpLogTx = mpsc::UnboundedSender<McpLogLine>;

impl McpLogLine {
    /// A raw line drained from the child process's `stderr` (stdio transport).
    /// The level is unknown, so it's tagged `stderr`; the text itself usually
    /// carries the server's own `ERROR`/`WARNING` marker.
    pub fn stderr(server: impl Into<String>, text: impl Into<String>) -> Self {
        Self { server: server.into(), level: Cow::Borrowed("stderr"), text: text.into() }
    }

    /// A connection lifecycle event (start failure, disconnect). Emitted for every
    /// transport so even HTTP servers — which have no `stderr` — get a connection
    /// history in their file.
    pub fn lifecycle(server: impl Into<String>, text: impl Into<String>) -> Self {
        Self { server: server.into(), level: Cow::Borrowed("lifecycle"), text: text.into() }
    }

    /// Builds a line from the `params` of an MCP `notifications/message`
    /// (`{ level, logger?, data }`). Missing `level` falls back to `info`; `data`
    /// strings pass through, other JSON is compacted to one line so nothing is
    /// lost and the file stays greppable.
    pub fn from_message(server: impl Into<String>, params: &Value) -> Self {
        let level = params.get("level").and_then(Value::as_str).unwrap_or("info").to_string();
        let logger = params.get("logger").and_then(Value::as_str);
        let text = format_message_data(logger, params.get("data"));
        Self { server: server.into(), level: Cow::Owned(level), text }
    }
}

/// Flattens the `data` of a `notifications/message` into one line, prefixing the
/// optional `logger` name. Strings pass through; any other JSON is compacted.
fn format_message_data(logger: Option<&str>, data: Option<&Value>) -> String {
    let body = match data {
        Some(Value::String(s)) => s.clone(),
        Some(v)                => serde_json::to_string(v).unwrap_or_else(|_| v.to_string()),
        None                   => String::new(),
    };
    match logger {
        Some(l) if !l.is_empty() => format!("{l}: {body}"),
        _                        => body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn from_message_uses_level_and_string_data() {
        let line = McpLogLine::from_message("srv", &json!({ "level": "error", "data": "boom" }));
        assert_eq!(line.level, "error");
        assert_eq!(line.text, "boom");
    }

    #[test]
    fn from_message_compacts_object_data_and_prefixes_logger() {
        // firecrawl-shaped payload.
        let params = json!({
            "level": "info",
            "logger": "scraper",
            "data": { "message": "Scraping URL", "context": { "url": "https://x/y" } }
        });
        let line = McpLogLine::from_message("firecrawl", &params);
        assert_eq!(line.level, "info");
        assert!(line.text.starts_with("scraper: "));
        assert!(line.text.contains("Scraping URL"));
        assert!(line.text.contains("https://x/y"));
    }

    #[test]
    fn from_message_defaults_level_to_info() {
        let line = McpLogLine::from_message("srv", &json!({ "data": "hi" }));
        assert_eq!(line.level, "info");
    }

    #[test]
    fn stderr_and_lifecycle_tags() {
        assert_eq!(McpLogLine::stderr("s", "x").level, "stderr");
        assert_eq!(McpLogLine::lifecycle("s", "x").level, "lifecycle");
    }
}
