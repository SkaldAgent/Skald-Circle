use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::tools::{
    SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
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

impl Tool for ReadFile {
    fn name(&self) -> &str { "read_file" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Read the content of a file with 1-based line numbers. \
         Use instead of cat/head/tail in the terminal. \
         Returns text prefixed as '  N | line'. When calling edit_file, copy the text after '| ' exactly. \
         For large files use start_line/end_line to read in chunks — files over ~2000 lines should never be read whole. \
         Use limit to cap output when end_line is unknown. \
         Paths under user-memory/ (private) or shared-memory/ (shared) read a note from your memory instead of disk."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type":        "string",
                    "description": "File path. Relative to project root, or absolute (e.g. /etc/hosts)."
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

    /// Routes `user-memory/…` / `shared-memory/…` to the note store; every other
    /// path falls through to the on-disk [`execute`](Self::execute).
    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        let path = super::path_arg(&args).unwrap_or_default();
        let Some(m) = classify_memory(&path) else { return self.run(args); };
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
