use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::tools::{
    SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
    truncate_label, MAX_LABEL_SHORT,
};
use super::{classify_memory, resolve, MemScope};

/// Appends text to the end of a file or note, creating it when absent.
///
/// Exists as its own tool rather than as a `insert_at_line` idiom for two
/// reasons. It is **atomic** on the memory path — one SQL statement, see
/// [`crate::db::memory_docs::append`] — where a read-modify-write would drop a
/// line under concurrent appends. And it **cannot destroy**: no argument of this
/// tool can shorten a file, which is what makes it safe to auto-allow on the
/// append-only `log.md` of shared memory (see `seed_fs_path_rules`) while every
/// other shared write still needs a human.
pub struct AppendFile {
    /// The `shared-memory` (system) pool; see [`ReadFile`](super::ReadFile).
    shared_pool: Arc<SqlitePool>,
}

impl AppendFile {
    pub fn new(shared_pool: Arc<SqlitePool>) -> Self { Self { shared_pool } }
}

/// Normalises appended text to whole lines: a trailing newline is added when
/// missing, so consecutive appends never run into one another. The *leading*
/// separator is the storage layer's job — it depends on how the existing content
/// ends, which only the writer can see atomically.
fn line_terminated(content: &str) -> String {
    if content.ends_with('\n') { content.to_string() } else { format!("{content}\n") }
}

impl Tool for AppendFile {
    fn name(&self) -> &str { "append_file" }
    fn display_name(&self) -> &str { "Append to File" }
    fn icon(&self) -> &str { "edit" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "Add text to the END of a file, creating the file if it does not exist. \
         Never reads, rewrites or shortens what is already there — use this for append-only files such as a log. \
         The text is written as whole lines: a newline is added before it if needed, and after it if missing. \
         Relative paths are resolved from your home directory (`~`); absolute paths (starting with /) are used as-is. \
         Works on user-memory/ and shared-memory/ notes as well as on disk."
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
                    "description": "Text to add at the end. May span multiple lines."
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
        truncate_label(&format!("append_file `{path}`"), MAX_LABEL_SHORT)
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
                anyhow::bail!("{path} is a memory root, not a note — append to a path like {path}/log.md");
            }
            let text = line_terminated(&content);
            crate::db::memory_docs::append(&pool, &rel, &text).await?;
            Ok(ToolResult::Text(format!("Appended {} bytes to {path}.", text.len())))
        })))
    }

    fn execute(&self, args: Value) -> Result<String> {
        use std::io::Write;

        let user_path = args["path"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: path"))?;
        let display = super::display_path_arg(&args);
        let content = args["content"].as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: content"))?;

        let abs = resolve(user_path)?;
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
        }

        // A file that does not end in a newline would otherwise glue the two
        // lines together — mirror the memory path's line discipline.
        let needs_sep = match std::fs::metadata(&abs) {
            Ok(md) if md.len() > 0 => {
                let mut tail = [0u8; 1];
                read_last_byte(&abs, &mut tail)?;
                tail[0] != b'\n'
            }
            _ => false,
        };

        let text = line_terminated(content);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&abs)
            .with_context(|| format!("Failed to open for append: {}", abs.display()))?;
        if needs_sep {
            f.write_all(b"\n")
                .with_context(|| format!("Failed to write: {}", abs.display()))?;
        }
        f.write_all(text.as_bytes())
            .with_context(|| format!("Failed to write: {}", abs.display()))?;

        Ok(format!("Appended {} bytes to {display}.", text.len()))
    }
}

/// Reads the final byte of `path` without loading the file — an append must not
/// pay for the size of what it appends to.
fn read_last_byte(path: &std::path::Path, buf: &mut [u8; 1]) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)
        .with_context(|| format!("Cannot read file: {}", path.display()))?;
    f.seek(SeekFrom::End(-1))?;
    f.read_exact(buf)?;
    Ok(())
}
