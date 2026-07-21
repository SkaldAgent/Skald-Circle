use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::tools::{
    SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
    truncate_label, MAX_LABEL_SHORT,
};
use super::{classify_memory, resolve, write_string, MemScope};

pub struct WriteFile {
    /// The `shared-memory` (system) pool; see [`ReadFile`](super::ReadFile).
    shared_pool: Arc<SqlitePool>,
}

impl WriteFile {
    pub fn new(shared_pool: Arc<SqlitePool>) -> Self { Self { shared_pool } }
}

impl Tool for WriteFile {
    fn name(&self) -> &str { "write_file" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Create a new file or fully overwrite an existing one. \
         Use instead of echo/cat heredoc in the terminal. \
         Relative paths are resolved from your home directory (`~`); absolute paths (starting with /) are used as-is. \
         OVERWRITES the entire file — for targeted edits to an existing file use edit_file instead. \
         Write Markdown under user-memory/ (private to you) or shared-memory/ (shared with everyone) to save a durable note in your memory instead of on disk."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type":        "string",
                    "description": "File path. Relative to `~` (your home), or absolute."
                },
                "content": {
                    "type":        "string",
                    "description": "Full content to write to the file."
                }
            },
            "required": ["path", "content"]
        })
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        super::path_arg(args)
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or("?");
        let _ = length;
        truncate_label(&format!("write_file `{path}`"), MAX_LABEL_SHORT)
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
        let content = args["content"].as_str().map(str::to_string);

        Box::new(SimpleExecution::new(Box::pin(async move {
            let content = content.ok_or_else(|| anyhow::anyhow!("Missing required argument: content"))?;
            if rel.is_empty() {
                anyhow::bail!("{path} is a memory root, not a note — write to a path like {path}/notes.md");
            }
            let existed = crate::db::memory_docs::get(&pool, &rel).await?.is_some();
            crate::db::memory_docs::upsert(&pool, &rel, &content).await?;
            let verb = if existed { "Overwrote" } else { "Created" };
            Ok(ToolResult::Text(format!("{verb} {path} ({} bytes).", content.len())))
        })))
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let content = args["content"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: content"))?;

        let abs = resolve(user_path)?;
        let existed = abs.exists();
        write_string(user_path, content)?;

        if existed {
            Ok(format!("Overwrote {user_path} ({} bytes).", content.len()))
        } else {
            Ok(format!("Created {user_path} ({} bytes).", content.len()))
        }
    }
}
