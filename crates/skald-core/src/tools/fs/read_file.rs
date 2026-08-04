use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::tools::{
    MediaRef, SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
    truncate_label, MAX_LABEL_SHORT, MAX_LABEL_FULL,
};
use super::{classify_memory, read_to_string, MemScope};

pub struct ReadFile {
    /// The `shared-memory` (system) pool. `user-memory` resolves per call from the
    /// `ToolContext`; only the shared store is a global singleton captured here.
    shared_pool: Arc<SqlitePool>,
}

impl ReadFile {
    pub fn new(shared_pool: Arc<SqlitePool>) -> Self { Self { shared_pool } }
}

/// Render `content` with 1-based line numbers, honouring the same
/// `start`/`end_line`/`limit` windowing as the disk path. Shared by the on-disk
/// [`ReadFile::execute`] and the `memory/` routing in [`ReadFile::run_with`].
fn number_lines(content: &str, start: usize, end_line: Option<usize>, limit: Option<usize>) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    let end = match (end_line, limit) {
        (Some(e), _)    => e.min(total),
        (None, Some(l)) => (start + l).min(total),
        (None, None)    => total,
    };

    if start >= total && total > 0 {
        return format!("(file has only {total} lines; start_line {} is out of range)", start + 1);
    }

    let end = end.max(start);
    let width = total.to_string().len().max(3);
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>width$} | {line}", start + i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A short, honest note returned as the `tool` message when `read_file` opens a
/// binary medium. The bytes travel out of band (`ToolResult::Media`); this text is
/// what the model reads in the tool result itself.
fn media_note(agent_path: &str, mime: &str, size: u64) -> String {
    format!(
        "[read_file: {agent_path} is binary media ({mime}, {}). It is provided to you directly as model input when the current model supports this format; it cannot be shown as text.]",
        human_size(size),
    )
}

/// `1536 → "1.5 KiB"`, `2_100_000 → "2.0 MiB"`.
fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= MIB { format!("{:.1} MiB", b / MIB) }
    else if b >= KIB { format!("{:.1} KiB", b / KIB) }
    else { format!("{bytes} B") }
}

impl Tool for ReadFile {
    fn name(&self) -> &str { "read_file" }
    fn display_name(&self) -> &str { "Read File" }
    fn icon(&self) -> &str { "read" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Read the content of a file with 1-based line numbers. \
         Use instead of cat/head/tail in the terminal. \
         Returns text prefixed as '  N | line'. When calling edit_file, copy the text after '| ' exactly. \
         For large files use start_line/end_line to read in chunks — files over ~2000 lines should never be read whole. \
         For an unfamiliar source file, call get_ast_outline first to get each definition's line range, then read \
         just the range you need instead of the whole file. \
         Use limit to cap output when end_line is unknown. \
         Paths under user-memory/ (private) or shared-memory/ (shared) read a note from your memory instead of disk."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type":        "string",
                    "description": "File path. Relative to `~` (your home), or absolute (e.g. /etc/hosts)."
                },
                "start_line": {
                    "type":        "integer",
                    "description": "First line to read (1-based, inclusive). Omit to start from the beginning."
                },
                "end_line": {
                    "type":        "integer",
                    "description": "Last line to read (1-based, inclusive). Omit to read to the end of the file."
                },
                "limit": {
                    "type":        "integer",
                    "description": "Maximum number of lines to return (max 2000). Applied after start_line when end_line is omitted.",
                    "maximum":     2000
                }
            },
            "required": ["path"]
        })
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        super::path_arg(args)
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        match length {
            ToolDescriptionLength::Short => {
                truncate_label(&format!("read_file `{path}`"), MAX_LABEL_SHORT)
            }
            ToolDescriptionLength::Full => {
                let range = match (args["start_line"].as_u64(), args["end_line"].as_u64()) {
                    (Some(s), Some(e)) => format!(" lines {s}-{e}"),
                    (Some(s), None)    => format!(" from line {s}"),
                    (None,    Some(e)) => format!(" to line {e}"),
                    _                  => String::new(),
                };
                truncate_label(&format!("read_file `{path}`{range}"), MAX_LABEL_FULL)
            }
        }
    }

    /// Routes `user-memory/…` / `shared-memory/…` to the note store; a physical
    /// path resolves to the caller's host workspace and is read there — as native
    /// media when it sniffs as an image/video/PDF (so a vision/document model can
    /// see it), otherwise as UTF-8 text with line numbers.
    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        let path = super::path_arg(&args).unwrap_or_default();
        let Some(m) = classify_memory(&path) else {
            // Physical path: resolve + containment-check up front (so an escape
            // fails immediately), then read inside the work future.
            let host = match super::resolve_target(&ctx.fs, &path) {
                Ok(super::FsTarget::Host(h)) => h,
                // A container-only path has no host file to sniff or to hand on
                // as a `MediaRef` (the shuttled copy is gone by the time the
                // projection would inline it), so it is read as text — same
                // windowing, same line numbers, via the shared `execute`.
                Ok(super::FsTarget::Container { .. }) => {
                    return super::run_physical(self, &ctx.fs, &path, args);
                }
                Err(e) => return super::error_exec(e.to_string()),
            };
            let start = args["start_line"].as_u64().map(|n| (n as usize).saturating_sub(1)).unwrap_or(0);
            let end_line = args["end_line"].as_u64().map(|n| n as usize);
            let limit = args["limit"].as_u64().map(|n| n.min(2000) as usize);
            return Box::new(SimpleExecution::new(Box::pin(async move {
                // A recognized medium is handed back for native inlining rather than
                // failing on non-UTF-8 bytes. We always emit the media (the message
                // builder gates on the resolved model's capability), so on a model
                // without the modality the note stands alone — never a decode error.
                if let Some(mime) = crate::session::handler::media::probe_media(&host).await {
                    let size = tokio::fs::metadata(&host).await.map(|m| m.len()).unwrap_or(0);
                    let host_str = host.to_string_lossy().into_owned();
                    return Ok(ToolResult::Media {
                        text:  media_note(&path, mime, size),
                        media: vec![MediaRef { host_path: host_str, mime: mime.to_string() }],
                    });
                }
                let content = tokio::fs::read_to_string(&host).await
                    .with_context(|| format!("Cannot read file: {path}"))?;
                Ok(ToolResult::Text(number_lines(&content, start, end_line, limit)))
            })));
        };
        let pool = match m.scope {
            MemScope::User   => Arc::clone(&ctx.pool),
            MemScope::Shared => Arc::clone(&self.shared_pool),
        };
        let rel = m.rel;
        let start = args["start_line"].as_u64().map(|n| (n as usize).saturating_sub(1)).unwrap_or(0);
        let end_line = args["end_line"].as_u64().map(|n| n as usize);
        let limit = args["limit"].as_u64().map(|n| n.min(2000) as usize);

        Box::new(SimpleExecution::new(Box::pin(async move {
            let Some(doc) = crate::db::memory_docs::get(&pool, &rel).await? else {
                anyhow::bail!("No note at {path}");
            };
            Ok(ToolResult::Text(number_lines(&doc.content, start, end_line, limit)))
        })))
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let content = read_to_string(user_path)?;
        let start = args["start_line"].as_u64().map(|n| (n as usize).saturating_sub(1)).unwrap_or(0);
        let end_line = args["end_line"].as_u64().map(|n| n as usize);
        let limit = args["limit"].as_u64().map(|n| n.min(2000) as usize);
        Ok(number_lines(&content, start, end_line, limit))
    }
}
