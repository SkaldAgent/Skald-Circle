use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::tools::{
    SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
    truncate_label, MAX_LABEL_SHORT, MAX_LABEL_FULL,
};
use super::{classify_memory, read_to_string, write_string, MemScope};

pub struct ReplaceLines {
    /// The `shared-memory` (system) pool; see [`ReadFile`](super::ReadFile).
    shared_pool: Arc<SqlitePool>,
}

impl ReplaceLines {
    pub fn new(shared_pool: Arc<SqlitePool>) -> Self { Self { shared_pool } }
}

/// Replaces the inclusive 1-based line range with `new`, returning the new content
/// and a result message. Shared by the on-disk `execute` and the `memory/` routing;
/// `display` is the path used in the message.
fn apply_replace(content: &str, args: &Value, display: &str) -> Result<(String, String)> {
    let from_line = args["from_line"].as_u64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: from_line"))? as usize;
    let to_line = args["to_line"].as_u64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: to_line"))? as usize;
    let new = args["new"].as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: new"))?;

    if from_line == 0 { anyhow::bail!("from_line must be >= 1"); }
    if to_line < from_line { anyhow::bail!("to_line must be >= from_line"); }

    let mut lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if from_line > total {
        anyhow::bail!("from_line {from_line} exceeds file length ({total} lines)");
    }
    let to_clamped = to_line.min(total);
    let new_lines: Vec<&str> = new.lines().collect();
    lines.splice((from_line - 1)..to_clamped, new_lines);

    let has_trailing = content.ends_with('\n');
    let mut updated = lines.join("\n");
    if has_trailing { updated.push('\n'); }

    let msg = format!(
        "Replaced lines {from_line}–{to_clamped} in {display} with {} new lines.",
        new.lines().count()
    );
    Ok((updated, msg))
}

impl Tool for ReplaceLines {
    fn name(&self) -> &str { "replace_lines" }
    fn display_name(&self) -> &str { "Edit File" }
    fn icon(&self) -> &str { "edit" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Replace a range of lines in a file with new text. \
         Relative paths are resolved from your home directory (`~`); absolute paths (starting with /) are used as-is. \
         Use the 1-based line numbers shown by read_file. `from_line` and `to_line` are inclusive."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":      { "type": "string",  "description": "File path. Relative to `~` (your home), or absolute." },
                "from_line": { "type": "integer", "description": "First line to replace (1-based, inclusive)." },
                "to_line":   { "type": "integer", "description": "Last line to replace (1-based, inclusive)." },
                "new":       { "type": "string",  "description": "Replacement text." }
            },
            "required": ["path", "from_line", "to_line", "new"]
        })
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        super::path_arg(args)
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        match length {
            ToolDescriptionLength::Short => {
                truncate_label(&format!("replace_lines `{path}`"), MAX_LABEL_SHORT)
            }
            ToolDescriptionLength::Full => {
                let from = args["from_line"].as_u64().map(|n| n.to_string()).unwrap_or_else(|| "?".into());
                let to   = args["to_line"].as_u64().map(|n| n.to_string()).unwrap_or_else(|| "?".into());
                truncate_label(&format!("replace_lines `{path}` lines {from}-{to}"), MAX_LABEL_FULL)
            }
        }
    }

    /// Routes `user-memory/…` / `shared-memory/…` to the note store; every other
    /// path falls through to the on-disk [`execute`](Self::execute).
    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        let path = super::path_arg(&args).unwrap_or_default();
        let Some(m) = classify_memory(&path) else {
            return match super::rewrite_to_host(&ctx.fs, &path, args) {
                Ok(args) => self.run(args),
                Err(e)   => super::error_exec(e.to_string()),
            };
        };
        let pool = match m.scope {
            MemScope::User   => Arc::clone(&ctx.pool),
            MemScope::Shared => Arc::clone(&self.shared_pool),
        };
        let rel = m.rel;

        Box::new(SimpleExecution::new(Box::pin(async move {
            let Some(doc) = crate::db::memory_docs::get(&pool, &rel).await? else {
                anyhow::bail!("No note at {path}");
            };
            let (updated, msg) = apply_replace(&doc.content, &args, &path)?;
            crate::db::memory_docs::upsert(&pool, &rel, &updated).await?;
            Ok(ToolResult::Text(msg))
        })))
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let content = read_to_string(user_path)?;
        let (updated, msg) = apply_replace(&content, &args, user_path)?;
        write_string(user_path, &updated)?;
        Ok(msg)
    }
}
