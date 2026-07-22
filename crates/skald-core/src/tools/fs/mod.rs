mod edit_file;
mod grep_files;
mod insert_at_line;
mod list_files;
mod memory_search;
mod read_file;
mod replace_lines;
mod search_file;
mod write_file;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::SqlitePool;

use core_api::user_fs::UserFs;

use crate::tools::{SimpleExecution, ToolExecution, ToolRegistry, ToolResult};

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
pub use memory_search::MemorySearch;
pub use read_file::ReadFile;
pub use replace_lines::ReplaceLines;
pub use search_file::SearchFile;
pub use write_file::WriteFile;

// ── Virtual memory namespace (blueprint §5) ───────────────────────────────────
//
// Two sibling top-level roots, each backed by the `memory_docs` table in SQLite
// rather than the disk. The fs-tools intercept these prefixes in `run_with` and
// route reads/writes to the `memory_docs` accessor on the right pool, so the LLM
// uses ordinary read/write/list against what looks like two folders.

/// The current user's **private** memory — routed to `ctx.pool` (`{userid}.db`,
/// behind SQLCipher).
pub const USER_MEMORY_ROOT: &str = "user-memory";

/// The instance-wide **shared** memory — routed to the system pool (`system.db`,
/// cleartext, readable by every member).
pub const SHARED_MEMORY_ROOT: &str = "shared-memory";

/// Which memory store a path resolves to.
pub enum MemScope {
    /// `user-memory/…` → the caller's own pool (`ToolContext::pool`).
    User,
    /// `shared-memory/…` → the shared system pool.
    Shared,
}

/// A path that falls inside the virtual memory namespace: the store it belongs to
/// and the note key **relative to that store's root** (the root prefix stripped).
pub struct MemRef {
    pub scope: MemScope,
    pub rel:   String,
}

/// Classifies a user-supplied path. Returns `Some` when it lands under one of the
/// virtual memory roots — to be routed to SQLite — and `None` for an ordinary
/// disk path.
///
/// The **first** component decides the store, taken raw *before* normalization, so
/// a `..` in the tail can never drop the memory root and silently fall back to a
/// disk path. The tail is then normalized (resolving `.`/`..`) and clamped at the
/// store root, so a memory path stays within its store and an absolute path is
/// always disk.
pub fn classify_memory(user_path: &str) -> Option<MemRef> {
    let mut parts = user_path.trim_start_matches("./").splitn(2, ['/', '\\']);
    let scope = match parts.next()? {
        USER_MEMORY_ROOT   => MemScope::User,
        SHARED_MEMORY_ROOT => MemScope::Shared,
        _ => return None,
    };
    // Normalize the tail within the store (empty = the root itself). `..` clamps
    // at the root rather than escaping upward.
    let tail = parts.next().unwrap_or("");
    let rel = lexical_normalize(Path::new(tail)).to_string_lossy().replace('\\', "/");
    Some(MemRef { scope, rel })
}

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
/// This closes `docs/../private/x` traversal and symlink escapes for both the allow
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

// ── Per-user physical routing (blueprint §6) ──────────────────────────────────
//
// A path that is *not* a memory path is physical: it resolves against the caller's
// private home (`~/…`) or a shared folder they belong to (`shared/{X}/…`), both on
// disk and bind-mounted into their container. The fs-tools run host-side, so we
// resolve to the host path here and hand the on-disk `execute` an absolute path.

/// Resolves a physical (non-memory) agent path to an absolute host path inside the
/// caller's workspace, **following symlinks and rejecting any escape** past the
/// mount root. This is the containment choke point: since the same tree is writable
/// from inside the container (`execute_cmd`), a symlink planted there that points
/// outside the home is caught by canonicalizing and prefix-checking against the base.
pub(crate) fn resolve_host_path(fs: &UserFs, agent_path: &str) -> Result<PathBuf> {
    let (base, tail) = fs.host_base_and_tail(agent_path).ok_or_else(|| {
        anyhow::anyhow!("no such shared folder, or you are not a member: {agent_path}")
    })?;
    // Canonicalize both sides so the prefix check is symlink-aware.
    let base_canon = canonicalize_for_policy(&base.to_string_lossy(), Path::new("/"));
    let joined = base.join(&tail);
    let canon = canonicalize_for_policy(&joined.to_string_lossy(), Path::new("/"));
    if !path_under(&canon, &base_canon) {
        anyhow::bail!("path escapes your workspace: {agent_path}");
    }
    Ok(canon)
}

/// Resolve a path arriving from the show-file / file-viewer surface into
/// `(host_abs, agent_display)`, scoped to the caller's workspace.
///
/// Accepts the agent vocabulary (`~/…`, `shared/{X}/…`, `projects/{O}/{S}/…`, bare
/// relative) **and** a container-absolute path (`/root/…`); any path outside the
/// caller's container view is rejected fail-closed. `agent_display` is the canonical
/// path the UI shows and echoes back to `/api/file`, so the tool, the viewer fetch
/// and the watcher all key on the same string. Memory paths (`user-memory/…`,
/// `shared-memory/…`) are virtual notes, not disk files — rejected with a clear error.
///
/// This is the single entry point the server shell uses for `show_file_to_user`,
/// `GET /api/file` and `GET /api/file/watch`; containment (canonicalize +
/// prefix-check, symlink-aware) is handled by [`resolve_host_path`].
pub fn resolve_view_path(fs: &UserFs, input: &str) -> Result<(PathBuf, String)> {
    if classify_memory(input).is_some() {
        anyhow::bail!("memory notes can't be opened in the file viewer: {input}");
    }
    let agent = fs.to_agent_display(input)
        .ok_or_else(|| anyhow::anyhow!("path is outside your workspace: {input}"))?;
    let host = resolve_host_path(fs, &agent)?;
    Ok((host, agent))
}

/// Rewrites the `path` argument of a physical fs-tool call to the resolved absolute
/// host path, so the on-disk `execute` (which takes absolute paths as-is) acts on
/// the caller's per-user workspace rather than the process working directory.
pub(crate) fn rewrite_to_host(fs: &UserFs, agent_path: &str, mut args: Value) -> Result<Value> {
    let host = resolve_host_path(fs, agent_path)?;
    args["path"] = Value::String(host.to_string_lossy().into_owned());
    Ok(args)
}

/// A tool execution that fails immediately — surfaces a containment / access error
/// from `run_with` without attempting a disk op.
pub(crate) fn error_exec<'a>(msg: String) -> Box<dyn ToolExecution + 'a> {
    Box::new(SimpleExecution::new(Box::pin(async move {
        Err::<ToolResult, _>(anyhow::anyhow!(msg))
    })))
}

/// Registers the filesystem tools. `shared_pool` is the system (`shared-memory`)
/// pool captured once here — a global singleton — and handed to the memory-aware
/// tools; each still resolves the per-user (`user-memory`) pool per call from the
/// `ToolContext`.
pub fn register_all(registry: &mut ToolRegistry, shared_pool: Arc<SqlitePool>) {
    registry.register(EditFile::new(Arc::clone(&shared_pool)));
    registry.register(GrepFiles::new()); // not memory-aware yet — see blueprint Prossimi passi
    registry.register(InsertAtLine::new(Arc::clone(&shared_pool)));
    registry.register(ListFiles::new(Arc::clone(&shared_pool)));
    registry.register(ReadFile::new(Arc::clone(&shared_pool)));
    registry.register(ReplaceLines::new(Arc::clone(&shared_pool)));
    registry.register(SearchFile::new(Arc::clone(&shared_pool)));
    registry.register(MemorySearch::new(Arc::clone(&shared_pool)));
    registry.register(WriteFile::new(shared_pool));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use serde_json::json;

    use core_api::user_fs::UserFs;

    use crate::tools::{ExecutionOutcome, Tool, ToolContext, ToolResult};

    /// A trivial workspace for the memory-routing tests, which never touch disk.
    fn test_fs() -> Arc<UserFs> {
        Arc::new(UserFs::new(
            "test",
            std::env::temp_dir().join("skald-fsmem-home"),
            "skald-test",
            PathBuf::from("/root"),
            vec![],
            vec![],
            None,
        ))
    }

    /// Physical path resolution + containment (blueprint §6): home and shared map
    /// to their host bases; a non-member shared folder, a `..` escape, and a
    /// symlink planted inside the home that points outside are all rejected.
    #[cfg(unix)]
    #[test]
    fn host_path_resolves_and_contains() {
        use core_api::user_fs::{ProjectMount, SharedMount};

        let root = std::env::temp_dir().join(format!("skald-fsroot-{}", std::process::id()));
        let home = root.join("homes").join("u1");
        let shared = root.join("shared").join("family");
        // Project owned by user `owner-id`, agent-visible as `projects/alice/budget`.
        let project = root.join("projects").join("owner-id").join("budget");
        let docs = root.join("docs");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&docs).unwrap();

        let fs = UserFs::new(
            "u1",
            home.clone(),
            "skald-u1",
            PathBuf::from("/root"),
            vec![SharedMount {
                name: "family".into(),
                host: shared.clone(),
                container: PathBuf::from("/root/shared/family"),
                can_write: true,
            }],
            vec![ProjectMount {
                owner_username: "alice".into(),
                slug: "budget".into(),
                host: project.clone(),
                container: PathBuf::from("/root/projects/alice/budget"),
                can_write: false,
            }],
            Some(docs.clone()),
        );

        let home_canon = canonicalize_for_policy(&home.to_string_lossy(), Path::new("/"));
        let shared_canon = canonicalize_for_policy(&shared.to_string_lossy(), Path::new("/"));
        let project_canon = canonicalize_for_policy(&project.to_string_lossy(), Path::new("/"));
        let docs_canon = canonicalize_for_policy(&docs.to_string_lossy(), Path::new("/"));

        // ~/… → private home (containment holds for a not-yet-existing file).
        let p = resolve_host_path(&fs, "~/notes.md").unwrap();
        assert!(path_under(&p, &home_canon), "{p:?}");
        // a bare relative path is home-relative too
        assert!(path_under(&resolve_host_path(&fs, "proj/main.rs").unwrap(), &home_canon));
        // shared/{member} → the shared host dir
        let s = resolve_host_path(&fs, "shared/family/list.md").unwrap();
        assert!(path_under(&s, &shared_canon), "{s:?}");
        // projects/{owner}/{slug} → the project host dir (two-segment routing)
        let pr = resolve_host_path(&fs, "projects/alice/budget/plan.md").unwrap();
        assert!(path_under(&pr, &project_canon), "{pr:?}");
        // docs/… → the shared read-only docs dir (both bare and ~-prefixed)
        let d = resolve_host_path(&fs, "docs/index.md").unwrap();
        assert!(path_under(&d, &docs_canon), "{d:?}");
        let d2 = resolve_host_path(&fs, "~/docs/index.md").unwrap();
        assert_eq!(d, d2);

        // a shared folder the user is NOT a member of → error
        assert!(resolve_host_path(&fs, "shared/secret/x.md").is_err());
        // a project the user cannot reach (wrong owner/slug) → error
        assert!(resolve_host_path(&fs, "projects/bob/budget/x.md").is_err());
        assert!(resolve_host_path(&fs, "projects/alice/secret/x.md").is_err());
        // `..` cannot climb out of the home
        assert!(resolve_host_path(&fs, "~/../u2/secret.md").is_err());

        // a symlink planted in the home that points outside is rejected: the
        // canonicalized target escapes the home base.
        std::os::unix::fs::symlink(&root, home.join("escape")).unwrap();
        assert!(resolve_host_path(&fs, "~/escape/homes/u2/secret.md").is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn classify_memory_splits_root_from_key() {
        let u = classify_memory("user-memory/notes/x.md").unwrap();
        assert!(matches!(u.scope, MemScope::User));
        assert_eq!(u.rel, "notes/x.md");

        let s = classify_memory("./shared-memory/casa.md").unwrap();
        assert!(matches!(s.scope, MemScope::Shared));
        assert_eq!(s.rel, "casa.md");

        // bare roots (with/without trailing slash) resolve to the empty key
        assert_eq!(classify_memory("user-memory").unwrap().rel, "");
        assert_eq!(classify_memory("shared-memory/").unwrap().rel, "");

        // `..` clamps inside the store instead of falling back to a disk path
        assert_eq!(classify_memory("user-memory/../secret.md").unwrap().rel, "secret.md");

        // ordinary, absolute, and look-alike paths are disk (None)
        assert!(classify_memory("src/main.rs").is_none());
        assert!(classify_memory("/etc/hosts").is_none());
        assert!(classify_memory("user-memoryish/x").is_none());
    }

    /// A throwaway owner-schema pool (as `Arc`, ready for a `ToolContext`), plus its
    /// dir for cleanup. `tag` + a counter keep parallel tests off the same file.
    async fn store(tag: &str) -> (Arc<SqlitePool>, PathBuf) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("skald-fsmem-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::create_user_pool(&dir.join("owner.db"), None).await.unwrap();
        (Arc::new(pool), dir)
    }

    /// Drives a tool through the context-aware path and returns its text result.
    async fn drive(tool: &dyn Tool, ctx: &ToolContext, args: Value) -> Result<String, String> {
        let exec = tool.run_with(ctx, args);
        match exec.wait().await {
            ExecutionOutcome::Completed(r) => Ok(r.to_wire()),
            ExecutionOutcome::Failed(e)    => Err(e),
            ExecutionOutcome::Cancelled    => Err("cancelled".into()),
        }
    }

    #[tokio::test]
    async fn memory_tools_route_and_isolate_user_vs_shared() {
        let (user,   udir) = store("user").await;
        let (shared, sdir) = store("shared").await;

        // The shared pool is captured by the tools; the user pool arrives per call.
        let write = WriteFile::new(Arc::clone(&shared));
        let read  = ReadFile::new(Arc::clone(&shared));
        let list  = ListFiles::new(Arc::clone(&shared));
        let ctx = ToolContext { session_id: 1, user_id: "u_test".into(), pool: Arc::clone(&user), fs: test_fs() };

        // Private write lands in the user pool — and never in the shared one.
        let out = drive(&write, &ctx, json!({"path":"user-memory/spesa.md","content":"latte\npane"}))
            .await.unwrap();
        assert!(out.starts_with("Created user-memory/spesa.md"), "{out}");
        assert!(crate::db::memory_docs::get(&user,   "spesa.md").await.unwrap().is_some());
        assert!(crate::db::memory_docs::get(&shared, "spesa.md").await.unwrap().is_none(),
                "a user-memory write must not touch the shared store");

        // Shared write lands in the shared pool — and never in the user one.
        drive(&write, &ctx, json!({"path":"shared-memory/casa.md","content":"wifi 1234"}))
            .await.unwrap();
        assert!(crate::db::memory_docs::get(&shared, "casa.md").await.unwrap().is_some());
        assert!(crate::db::memory_docs::get(&user,   "casa.md").await.unwrap().is_none());

        // Read back with 1-based line numbers; a missing note errors.
        let r = drive(&read, &ctx, json!({"path":"user-memory/spesa.md"})).await.unwrap();
        assert!(r.contains("| latte") && r.contains("| pane"), "{r}");
        assert!(drive(&read, &ctx, json!({"path":"user-memory/nope.md"})).await.is_err());

        // A second write to the same key overwrites (and says so).
        let out = drive(&write, &ctx, json!({"path":"user-memory/spesa.md","content":"latte"}))
            .await.unwrap();
        assert!(out.starts_with("Overwrote user-memory/spesa.md"), "{out}");

        // Listing returns keys relative to the requested directory.
        drive(&write, &ctx, json!({"path":"user-memory/notes/idee.md","content":"x"}))
            .await.unwrap();
        let l = drive(&list, &ctx, json!({"path":"user-memory"})).await.unwrap();
        assert_eq!(serde_json::from_str::<Vec<String>>(&l).unwrap(),
                   vec!["notes/idee.md".to_string(), "spesa.md".to_string()]);
        let l = drive(&list, &ctx, json!({"path":"user-memory/notes"})).await.unwrap();
        assert_eq!(serde_json::from_str::<Vec<String>>(&l).unwrap(),
                   vec!["idee.md".to_string()]);

        let _ = std::fs::remove_dir_all(&udir);
        let _ = std::fs::remove_dir_all(&sdir);
    }

    #[tokio::test]
    async fn memory_edit_insert_replace_search_route_to_the_note() {
        let (user,   udir) = store("edit-user").await;
        let (shared, sdir) = store("edit-shared").await;

        let write   = WriteFile::new(Arc::clone(&shared));
        let edit    = EditFile::new(Arc::clone(&shared));
        let insert  = InsertAtLine::new(Arc::clone(&shared));
        let replace = ReplaceLines::new(Arc::clone(&shared));
        let search  = SearchFile::new(Arc::clone(&shared));
        let ctx = ToolContext { session_id: 1, user_id: "u_test".into(), pool: Arc::clone(&user), fs: test_fs() };

        async fn note(pool: &SqlitePool, path: &str) -> String {
            crate::db::memory_docs::get(pool, path).await.unwrap().unwrap().content
        }

        drive(&write, &ctx, json!({"path":"user-memory/todo.md","content":"latte\npane\nuvoa"}))
            .await.unwrap();

        // edit_file: fix the typo, in place
        let out = drive(&edit, &ctx, json!({"path":"user-memory/todo.md","old":"uvoa","new":"uova"}))
            .await.unwrap();
        assert_eq!(out, "Edited user-memory/todo.md.");
        assert_eq!(note(&user, "todo.md").await, "latte\npane\nuova");

        // insert_at_line: add a line after line 1
        drive(&insert, &ctx, json!({"path":"user-memory/todo.md","line":1,"content":"burro","placement":"after"}))
            .await.unwrap();
        assert_eq!(note(&user, "todo.md").await, "latte\nburro\npane\nuova");

        // replace_lines: collapse lines 2–3 into one
        drive(&replace, &ctx, json!({"path":"user-memory/todo.md","from_line":2,"to_line":3,"new":"olio"}))
            .await.unwrap();
        assert_eq!(note(&user, "todo.md").await, "latte\nolio\nuova");

        // search_file: find a line inside the note
        let s = drive(&search, &ctx, json!({"path":"user-memory/todo.md","query":"olio"})).await.unwrap();
        assert!(s.contains("match(es) in user-memory/todo.md"), "{s}");
        assert!(s.contains("| olio"), "{s}");

        // editing a note that doesn't exist errors, not creates
        assert!(drive(&edit, &ctx, json!({"path":"user-memory/ghost.md","old":"a","new":"b"}))
            .await.is_err());

        let _ = std::fs::remove_dir_all(&udir);
        let _ = std::fs::remove_dir_all(&sdir);
    }

    #[tokio::test]
    async fn memory_search_scopes_to_user_shared_or_all() {
        let (user,   udir) = store("search-user").await;
        let (shared, sdir) = store("search-shared").await;

        let write  = WriteFile::new(Arc::clone(&shared));
        let search = MemorySearch::new(Arc::clone(&shared));
        let ctx = ToolContext { session_id: 1, user_id: "u_test".into(), pool: Arc::clone(&user), fs: test_fs() };

        // one note in each store, both mentioning "wifi"
        drive(&write, &ctx, json!({"path":"user-memory/rete.md","content":"la mia wifi privata"}))
            .await.unwrap();
        drive(&write, &ctx, json!({"path":"shared-memory/casa.md","content":"wifi di casa 1234"}))
            .await.unwrap();

        // scope=private → only the user store
        let r = drive(&search, &ctx, json!({"query":"wifi","scope":"private"})).await.unwrap();
        assert!(r.contains("[user-memory] rete.md"), "{r}");
        assert!(!r.contains("shared-memory"), "{r}");

        // scope=shared → only the shared store
        let r = drive(&search, &ctx, json!({"query":"wifi","scope":"shared"})).await.unwrap();
        assert!(r.contains("[shared-memory] casa.md"), "{r}");
        assert!(!r.contains("[user-memory]"), "{r}");

        // scope=all (default) → both, and the snippet highlights the term
        let r = drive(&search, &ctx, json!({"query":"wifi"})).await.unwrap();
        assert!(r.contains("[user-memory] rete.md") && r.contains("[shared-memory] casa.md"), "{r}");
        assert!(r.contains("[wifi]"), "snippet should highlight the match: {r}");

        // no match → a friendly message, not an error
        let r = drive(&search, &ctx, json!({"query":"inesistente"})).await.unwrap();
        assert!(r.starts_with("No memory notes match"), "{r}");

        let _ = std::fs::remove_dir_all(&udir);
        let _ = std::fs::remove_dir_all(&sdir);
    }

    /// A physical `read_file` on a binary image hands the file back as
    /// `ToolResult::Media` (host path + sniffed MIME) instead of failing on the
    /// non-UTF-8 bytes; a UTF-8 file still reads as line-numbered text.
    #[tokio::test]
    async fn read_file_returns_media_for_binary_image() {
        let (shared, sdir) = store("readmedia-shared").await;
        let (user,   udir) = store("readmedia-user").await;

        let root = std::env::temp_dir().join(format!("skald-readmedia-{}", uuid::Uuid::new_v4()));
        let home = root.join("homes").join("u1");
        std::fs::create_dir_all(&home).unwrap();
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[0xAA; 64]);
        std::fs::write(home.join("pic.png"), &png).unwrap();
        std::fs::write(home.join("note.txt"), "hello\nworld").unwrap();

        let fs = Arc::new(UserFs::new(
            "u1", home.clone(), "skald-u1", PathBuf::from("/root"), vec![], vec![], None,
        ));
        let ctx = ToolContext { session_id: 1, user_id: "u1".into(), pool: Arc::clone(&user), fs };
        let read = ReadFile::new(Arc::clone(&shared));

        // image → Media, carrying the resolved host path + MIME.
        match read.run_with(&ctx, json!({"path": "~/pic.png"})).wait().await {
            ExecutionOutcome::Completed(ToolResult::Media { text, media }) => {
                assert!(text.contains("binary media") && text.contains("image/png"), "{text}");
                assert_eq!(media.len(), 1);
                assert_eq!(media[0].mime, "image/png");
                assert!(media[0].host_path.ends_with("pic.png"), "{}", media[0].host_path);
            }
            other => panic!("expected Media, got {other:?}"),
        }

        // UTF-8 text → ordinary numbered text.
        match read.run_with(&ctx, json!({"path": "~/note.txt"})).wait().await {
            ExecutionOutcome::Completed(ToolResult::Text(t)) => {
                assert!(t.contains("| hello") && t.contains("| world"), "{t}");
            }
            other => panic!("expected Text, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&udir);
        let _ = std::fs::remove_dir_all(&sdir);
    }
}
