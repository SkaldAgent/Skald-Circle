use std::path::Path;

use anyhow::Result;
use serde_json::{Value, json};

use crate::core::tools::{Tool, ToolDescriptionLength, truncate_label, MAX_LABEL_SHORT};
use super::resolve;

/// Directories to skip unconditionally when walking.
/// `secrets` is skipped so a recursive listing rooted at a parent (e.g. the auto-read
/// working directory) never reveals the contents of the secrets store.
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".cache", "secrets"];

pub struct ListFiles;

impl ListFiles {
    pub fn new() -> Self { Self }
}

impl Tool for ListFiles {
    fn name(&self) -> &str { "list_files" }
    fn category(&self) -> crate::core::tools::ToolCategory { crate::core::tools::ToolCategory::Filesystem }

    fn description(&self) -> &str {
        "List files and directories under a path. \
         Use instead of ls/find in the terminal. \
         Relative paths are resolved from the project root; absolute paths (starting with /) are used as-is. \
         Skips .git, target, node_modules, .cache. \
         Returns a JSON array of paths relative to the requested directory. \
         Use depth=1 for immediate contents only, depth=2-3 for moderate exploration."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type":        "string",
                    "description": "Directory to list. Defaults to project root if omitted."
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
