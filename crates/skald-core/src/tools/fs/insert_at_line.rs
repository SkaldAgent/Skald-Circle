use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::tools::{
    SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
    truncate_label, MAX_LABEL_SHORT, MAX_LABEL_FULL,
};
use super::{classify_memory, read_to_string, write_string, MemScope};

pub struct InsertAtLine {
    /// The `shared-memory` (system) pool; see [`ReadFile`](super::ReadFile).
    shared_pool: Arc<SqlitePool>,
}

impl InsertAtLine {
    pub fn new(shared_pool: Arc<SqlitePool>) -> Self { Self { shared_pool } }
}

/// Inserts `content` before/after `line` in `text`, returning the new text and a
/// result message. Shared by the on-disk `execute` and the `memory/` routing;
/// `display` is the path used in the message.
fn apply_insert(text: &str, args: &Value, display: &str) -> Result<(String, String)> {
    let line_num = args["line"].as_u64()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: line"))? as usize;
    let new_text = args["content"].as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: content"))?;
    let placement = args["placement"].as_str().unwrap_or("after");

    anyhow::ensure!(line_num >= 1, "line must be >= 1");

    let mut lines: Vec<&str> = text.split('\n').collect();
    let idx        = (line_num - 1).min(lines.len().saturating_sub(1));
    let insert_idx = if placement == "before" { idx } else { idx + 1 };
    let new_lines: Vec<&str> = new_text.split('\n').collect();
    for (i, l) in new_lines.iter().enumerate() {
        lines.insert(insert_idx + i, l);
    }
    let updated = lines.join("\n");
    let msg = format!(
        "Inserted {} line(s) {} line {} in {display}.",
        new_lines.len(), placement, line_num
    );
    Ok((updated, msg))
}

impl Tool for InsertAtLine {
    fn name(&self) -> &str { "insert_at_line" }
    fn display_name(&self) -> &str { "Edit File" }
    fn icon(&self) -> &str { "edit" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Insert new text immediately before or after a specific line number in a file. \
         Relative paths are resolved from your home directory (`~`); absolute paths (starting with /) are used as-is."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":    { "type": "string", "description": "File path. Relative to `~` (your home), or absolute." },
                "line":    { "type": "integer", "minimum": 1, "description": "1-based line number." },
                "content": { "type": "string",  "description": "Text to insert. May span multiple lines." },
                "placement": {
                    "type": "string",
                    "enum": ["before", "after"],
                    "description": "Whether to insert before or after the target line. Default: \"after\"."
                }
            },
            "required": ["path", "line", "content"]
        })
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        super::path_arg(args)
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        match length {
            ToolDescriptionLength::Short => {
                truncate_label(&format!("insert_at_line `{path}`"), MAX_LABEL_SHORT)
            }
            ToolDescriptionLength::Full => {
                let line = args["line"].as_u64().map(|n| format!(" line {n}")).unwrap_or_default();
                truncate_label(&format!("insert_at_line `{path}`{line}"), MAX_LABEL_FULL)
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
            let (updated, msg) = apply_insert(&doc.content, &args, &path)?;
            crate::db::memory_docs::upsert(&pool, &rel, &updated).await?;
            Ok(ToolResult::Text(msg))
        })))
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let display = super::display_path_arg(&args);
        let text = read_to_string(user_path)?;
        let (updated, msg) = apply_insert(&text, &args, display)?;
        write_string(user_path, &updated)?;
        Ok(msg)
    }
}
