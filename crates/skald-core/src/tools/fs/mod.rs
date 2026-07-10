mod edit_file;
mod grep_files;
mod insert_at_line;
mod list_files;
mod read_file;
mod replace_lines;
mod search_file;
mod write_file;

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::tools::ToolRegistry;

/// Extracts the `path` argument as an owned string, if present. Single-file
/// tools use this to advertise their target to the UI via `Tool::target_path`,
/// keeping the argument name in one place.
pub(crate) fn path_arg(args: &Value) -> Option<String> {
    args.get("path").and_then(Value::as_str).map(str::to_string)
}

pub use edit_file::EditFile;
pub use grep_files::GrepFiles;
pub use insert_at_line::InsertAtLine;
pub use list_files::ListFiles;
pub use read_file::ReadFile;
pub use replace_lines::ReplaceLines;
pub use search_file::SearchFile;
pub use write_file::WriteFile;

/// Resolve a user-supplied path:
/// - starts with `/`  → absolute path, used as-is
/// - otherwise        → relative to the process working directory (project root)
pub fn resolve(user_path: &str) -> Result<PathBuf> {
    let p = PathBuf::from(user_path);
    if p.is_absolute() {
        Ok(p)
    } else {
        let cwd = std::env::current_dir()
            .context("Failed to read current working directory")?;
        Ok(cwd.join(p))
    }
}

/// Resolves `path` (relative entries against `base`) to an absolute, canonical form
/// suitable for security prefix-matching. `.`/`..` are resolved and symlinks in the
/// existing portion of the path are followed: the longest existing ancestor is
/// canonicalized via the OS, and any not-yet-existing tail (e.g. a write target that
/// does not exist yet) is appended lexically. Falls back to a pure lexical normalization
/// when nothing along the path can be canonicalized.
///
/// This closes `docs/../secrets/x` traversal and symlink escapes for both the allow
/// fast-paths (`RunContext`) and the deny rules (`approval::normalize_path`).
pub fn canonicalize_for_policy(path: &str, base: &Path) -> PathBuf {
    let raw = {
        let p = Path::new(path);
        if p.is_absolute() { p.to_path_buf() } else { base.join(p) }
    };
    let cleaned = lexical_normalize(&raw);

    // Longest existing ancestor first (ancestors() yields self, then parents).
    for ancestor in cleaned.ancestors() {
        if let Ok(canon) = std::fs::canonicalize(ancestor) {
            // `canon.join("")` appends a trailing separator, so skip the join
            // when the tail is empty (the common case: the file itself is its
            // first canonicalizable ancestor). Otherwise the canonical path
            // ends in '/', leaking into display strings and /api/file requests.
            return match cleaned.strip_prefix(ancestor) {
                Ok(tail) if !tail.as_os_str().is_empty() => canon.join(tail),
                _                                         => canon,
            };
        }
    }
    cleaned
}

/// Pure lexical normalization: resolves `.` and `..` components without touching the
/// filesystem. Used as the base for `canonicalize_for_policy` and as its fallback.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => { out.pop(); }
            Component::CurDir    => {}
            other                => out.push(other.as_os_str()),
        }
    }
    out
}

/// True if `child` is `base` itself or lies inside it. Both should already be canonical
/// (e.g. produced by `canonicalize_for_policy`). Comparison is component-wise, so
/// `/a/bc` is not considered to be under `/a/b`.
pub fn path_under(child: &Path, base: &Path) -> bool {
    child.starts_with(base)
}

/// Normalize a user path for display in the UI: relative to the project root when the
/// file lives inside it, absolute otherwise. Resolves `.`/`..` and symlinks via
/// `canonicalize_for_policy` so the same file always yields the same string — keeping
/// the file viewer's "already loaded" check and its watcher subscription consistent.
pub fn relativize_for_display(user_path: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let abs = canonicalize_for_policy(user_path, &cwd);
    let cwd_canon = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    match abs.strip_prefix(&cwd_canon) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_)  => abs.to_string_lossy().into_owned(),
    }
}

pub(super) fn read_to_string(user_path: &str) -> Result<String> {
    let abs = resolve(user_path)?;
    std::fs::read_to_string(&abs)
        .with_context(|| format!("Cannot read file: {user_path}"))
}

pub(super) fn write_string(user_path: &str, content: &str) -> Result<()> {
    let abs = resolve(user_path)?;
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
    }
    std::fs::write(&abs, content)
        .with_context(|| format!("Failed to write: {}", abs.display()))
}

pub fn register_all(registry: &mut ToolRegistry) {
    registry.register(EditFile::new());
    registry.register(GrepFiles::new());
    registry.register(InsertAtLine::new());
    registry.register(ListFiles::new());
    registry.register(ReadFile::new());
    registry.register(ReplaceLines::new());
    registry.register(SearchFile::new());
    registry.register(WriteFile::new());
}
