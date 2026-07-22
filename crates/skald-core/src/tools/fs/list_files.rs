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
         Set with_metadata=true to instead return objects {path, line_count?, size} — handy for spotting a \
         large file worth outlining with get_ast_outline before you read it. \
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
                },
                "with_metadata": {
                    "type":        "boolean",
                    "description": "If true, return objects {path, line_count?, size} instead of bare path strings. size is human-readable; line_count is included only for text files (omitted for binaries and very large files). Use it to decide whether to get_ast_outline a large file before reading it."
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
        let with_metadata = args["with_metadata"].as_bool().unwrap_or(false);
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

            if with_metadata {
                let mut entries: Vec<FileEntry> = crate::db::memory_docs::list_with_metadata(&pool, &prefix)
                    .await?
                    .into_iter()
                    .map(|e| FileEntry {
                        path:       e.path.strip_prefix(&prefix).unwrap_or(&e.path).to_string(),
                        line_count: Some(e.line_count.max(0) as usize),
                        size:       Some(human_size(e.byte_len.max(0) as u64)),
                    })
                    .collect();
                entries.sort_by(|a, b| a.path.cmp(&b.path));
                return Ok(ToolResult::Text(serde_json::to_string(&entries)?));
            }

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
        let with_metadata = args["with_metadata"].as_bool().unwrap_or(false);
        let dir = resolve(user_path)?;

        let mut paths: Vec<String> = Vec::new();
        walk(&dir, &dir, 0, max_depth, dirs_only, &mut paths)?;
        paths.sort();

        if !with_metadata {
            return Ok(serde_json::to_string(&paths)?);
        }
        let entries: Vec<FileEntry> = paths.into_iter()
            .map(|rel| file_entry(&dir.join(&rel), rel))
            .collect();
        Ok(serde_json::to_string(&entries)?)
    }
}

/// A `with_metadata` listing row. Field order (declaration order) is the wire
/// order; `line_count` and `size` are omitted when unavailable.
#[derive(serde::Serialize)]
struct FileEntry {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    line_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<String>,
}

/// Largest file we'll read to count lines; bigger files report `size` only, so
/// a metadata listing never turns into a full read of the tree.
const LINE_COUNT_SIZE_CAP: u64 = 2 * 1024 * 1024;

/// Build a metadata row for one on-disk path. `size` comes free from a stat;
/// `line_count` is read only for text files within the size cap.
fn file_entry(abs: &Path, rel: String) -> FileEntry {
    let meta = std::fs::metadata(abs).ok();
    let len = meta.as_ref().map(|m| m.len());
    let is_file = meta.as_ref().map(|m| m.is_file()).unwrap_or(false);
    let line_count = match len {
        Some(l) if is_file && l <= LINE_COUNT_SIZE_CAP => count_lines_if_text(abs),
        _ => None,
    };
    FileEntry { path: rel, line_count, size: len.map(human_size) }
}

/// Line count of a text file, or `None` if it reads as binary (contains a NUL).
fn count_lines_if_text(abs: &Path) -> Option<usize> {
    let bytes = std::fs::read(abs).ok()?;
    if bytes.contains(&0) { return None; }
    Some(count_lines(&bytes))
}

/// Number of lines an editor would show: 0 for empty, else newline-count plus
/// one when the file does not end in a newline.
fn count_lines(bytes: &[u8]) -> usize {
    if bytes.is_empty() { return 0; }
    let nl = bytes.iter().filter(|&&b| b == b'\n').count();
    if bytes.last() == Some(&b'\n') { nl } else { nl + 1 }
}

/// Human-readable byte size, `ls -h` style (base 1024): "512 B", "18 KB", "1.4 MB".
fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        let s = format!("{size:.1}");
        let s = s.strip_suffix(".0").unwrap_or(&s);
        format!("{s} {}", UNITS[unit])
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
