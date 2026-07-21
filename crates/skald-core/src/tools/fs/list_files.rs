use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::tools::{
    SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
    truncate_label, MAX_LABEL_SHORT,
};
use super::{classify_memory, resolve, MemScope};

/// Directories to skip unconditionally when walking — noise, not policy.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".cache"];

pub struct ListFiles {
    /// The `shared-memory` (system) pool; see [`ReadFile`](super::ReadFile).
    shared_pool: Arc<SqlitePool>,
}

impl ListFiles {
    pub fn new(shared_pool: Arc<SqlitePool>) -> Self { Self { shared_pool } }
}

impl Tool for ListFiles {
    fn name(&self) -> &str { "list_files" }
    fn display_name(&self) -> &str { "List Files" }
    fn icon(&self) -> &str { "list" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "List files and directories under a path. \
         Use instead of ls/find in the terminal. \
         Relative paths are resolved from your home directory (`~`); absolute paths (starting with /) are used as-is. \
         Skips .git, target, node_modules, .cache. \
         Returns a JSON array of paths relative to the requested directory. \
         Use depth=1 for immediate contents only, depth=2-3 for moderate exploration. \
         Listing under user-memory/ (private) or shared-memory/ (shared) lists your memory notes instead of disk."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type":        "string",
                    "description": "Directory to list. Defaults to `~` (your home) if omitted."
                },
                "depth": {
                    "type":        "integer",
                    "description": "Maximum recursion depth (default 3). Use 1 for immediate contents only."
                },
                "dirs_only": {
                    "type":        "boolean",
                    "description": "If true, return only directories and omit files (default false)."
                }
            }
        })
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let path = args["path"].as_str().unwrap_or(".");
        let _ = length;
        truncate_label(&format!("list_files `{path}`"), MAX_LABEL_SHORT)
    }

    /// Routes `user-memory/…` / `shared-memory/…` to the note store; every other
    /// path falls through to the on-disk [`execute`](Self::execute). Memory is a
    /// flat key space, so `depth`/`dirs_only` don't apply — the whole subtree
    /// under the prefix is returned, keyed relative to the requested directory.
    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        let path = args["path"].as_str().unwrap_or("").to_string();
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
            // Treat `rel` as a directory prefix: match `rel/…` (or everything at
            // the root), then strip it so results are relative to what was asked.
            let prefix = if rel.is_empty() || rel.ends_with('/') { rel } else { format!("{rel}/") };
            let entries = crate::db::memory_docs::list(&pool, &prefix).await?;
            let mut paths: Vec<String> = entries.into_iter()
                .map(|e| e.path.strip_prefix(&prefix).unwrap_or(&e.path).to_string())
                .collect();
            paths.sort();
            Ok(ToolResult::Text(serde_json::to_string(&paths)?))
        })))
    }

    fn execute(&self, args: Value) -> Result<String> {
        let user_path = args["path"].as_str().unwrap_or(".");
        let max_depth = args["depth"].as_u64().unwrap_or(3) as usize;
        let dirs_only = args["dirs_only"].as_bool().unwrap_or(false);
        let dir = resolve(user_path)?;

        let mut paths: Vec<String> = Vec::new();
        walk(&dir, &dir, 0, max_depth, dirs_only, &mut paths)?;
        paths.sort();
        Ok(serde_json::to_string(&paths)?)
    }
}

fn walk(root: &Path, dir: &Path, depth: usize, max_depth: usize, dirs_only: bool, out: &mut Vec<String>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if path.is_dir() {
            if SKIP_DIRS.contains(&name) { continue; }
            if dirs_only {
                let rel = path.strip_prefix(root)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| path.to_string_lossy().to_string());
                out.push(rel);
            }
            if depth + 1 < max_depth {
                walk(root, &path, depth + 1, max_depth, dirs_only, out)?;
            }
        } else if path.is_file() && !dirs_only {
            let rel = path.strip_prefix(root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| path.to_string_lossy().to_string());
            out.push(rel);
        }
    }
    Ok(())
}
