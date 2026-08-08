mod append_file;
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

use core_api::user_fs::{RouteError, UserFs};

use crate::tools::{SimpleExecution, ToolExecution, ToolRegistry, ToolResult};

/// Extracts the `path` argument as an owned string, if present. Single-file
/// tools use this to advertise their target to the UI via `Tool::target_path`,
/// keeping the argument name in one place.
pub(crate) fn path_arg(args: &Value) -> Option<String> {
    args.get("path").and_then(Value::as_str).map(str::to_string)
}

pub use append_file::AppendFile;
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

/// Strips the ways an agent spells "in my home" — `./`, `~/`, the container-absolute
/// `{CONTAINER_HOME}/` — so the memory roots are recognised whichever spelling the
/// model reaches for.
///
/// Without this, `~/user-memory/x.md` misses the match below and falls through to the
/// **disk** router, which resolves it against the caller's home: the note lands in a
/// physical `user-memory/` directory that no tool ever reads back, since every reader
/// (`read_file`, `list_files`, `memory_search`, the lints, the viewer) goes to
/// `memory_docs`. Silent data loss, and the kind an agent then re-confirms by `ls`.
fn strip_home_spelling(user_path: &str) -> &str {
    let p = user_path.trim_start_matches("./");
    if let Some(rest) = p.strip_prefix("~/") {
        return rest;
    }
    // `/root/user-memory/…`, but not `/rootless/…` — the separator is required.
    p.strip_prefix(crate::container::CONTAINER_HOME)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(p)
}

/// Classifies a user-supplied path. Returns `Some` when it lands under one of the
/// virtual memory roots — to be routed to SQLite — and `None` for an ordinary
/// disk path.
///
/// The **first** component decides the store, taken raw *before* normalization (bar
/// the home spelling, see [`strip_home_spelling`]), so a `..` in the tail can never
/// drop the memory root and silently fall back to a disk path. The tail is then
/// normalized (resolving `.`/`..`) and clamped at the store root, so a memory path
/// stays within its store.
pub fn classify_memory(user_path: &str) -> Option<MemRef> {
    let mut parts = strip_home_spelling(user_path).splitn(2, ['/', '\\']);
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
    let (base, tail) = match fs.host_base_and_tail(agent_path) {
        Ok(pair) => pair,
        Err(RouteError::Denied(msg)) => anyhow::bail!(msg),
        Err(RouteError::SkillAlias { id, tail }) => resolve_skill_alias(fs, &id, &tail)?,
    };
    // Canonicalize both sides so the prefix check is symlink-aware.
    let base_canon = canonicalize_for_policy(&base.to_string_lossy(), Path::new("/"));
    let joined = base.join(&tail);
    let canon = canonicalize_for_policy(&joined.to_string_lossy(), Path::new("/"));
    if !path_under(&canon, &base_canon) {
        anyhow::bail!("path escapes your workspace: {agent_path}");
    }
    Ok(canon)
}

/// Resolves the tolerant bare-id alias `skills/<id>/…` — the shortest spelling of a
/// skill path, and therefore the one a model reaches for on its own, both out of
/// habit and because skill bodies written elsewhere cite it that way.
///
/// It resolves **only when the id lives in exactly one** of the two trees. A
/// collision fails loudly, listing both full paths, rather than letting either win:
/// the personal tree winning would mean a silent divergence from the group's set,
/// the group's winning would mean the member's own work is ignored, and neither is
/// something to decide behind the model's back. With the full path printed in the
/// index the disambiguation is free anyway — they are two different lines.
///
/// The root itself is probed last, so the signpost `skills/README.md` reads like any
/// other file rather than being the one path in the tree that fails.
fn resolve_skill_alias(fs: &UserFs, id: &str, tail: &str) -> Result<(PathBuf, String)> {
    let found: Vec<(String, PathBuf)> = fs
        .skill_alias_candidates(id)
        .into_iter()
        .filter(|(_, host)| host.is_dir())
        .collect();

    match found.len() {
        1 => {
            let (_, host) = found.into_iter().next().expect("len checked");
            Ok((host, tail.to_string()))
        }
        0 => {
            // Not a skill id. It may still be something in the root mount — the
            // signpost README — before it is nothing at all.
            if let Some(sk) = fs.skills.as_ref().filter(|sk| sk.root_host.join(id).exists()) {
                return Ok((sk.root_host.clone(), agent_join_str(id, tail)));
            }
            anyhow::bail!(fs.skill_route_hint(id))
        }
        _ => {
            let paths: Vec<String> = found.into_iter().map(|(agent, _)| agent).collect();
            anyhow::bail!(
                "`skills/{id}` is ambiguous — that id exists in more than one place. \
                 Use the full path: {}",
                paths.join(" or ")
            )
        }
    }
}

/// Joins a first segment with a possibly-empty tail, for a path relative to a base.
fn agent_join_str(head: &str, tail: &str) -> String {
    if tail.is_empty() { head.to_string() } else { format!("{head}/{tail}") }
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
    match resolve_view_target(fs, input)? {
        (FsTarget::Host(host), agent) => Ok((host, agent)),
        (FsTarget::Container { .. }, agent) => anyhow::bail!(
            "{agent} lives only inside your container; this action needs a file in your \
             mounted folders (~, shared/, projects/)"
        ),
    }
}

/// The view-surface twin of [`resolve_target`]: normalizes an incoming path to the
/// agent vocabulary and says where it lives, so the viewer can open a
/// container-only path (`/tmp/report.pdf`) the same way the agent reads it.
///
/// A container-only path has no agent-vocabulary spelling — it *is* its own
/// display form, which is also what `show_file_to_user` echoes back.
pub fn resolve_view_target(fs: &UserFs, input: &str) -> Result<(FsTarget, String)> {
    if classify_memory(input).is_some() {
        anyhow::bail!("memory notes can't be opened in the file viewer: {input}");
    }
    let raw = Path::new(input);
    if raw.is_absolute() && fs.container_to_agent(raw).is_none() {
        let path = lexical_normalize(raw);
        let display = path.to_string_lossy().into_owned();
        return Ok((
            FsTarget::Container { container: fs.container_name.clone(), path },
            display,
        ));
    }
    let agent = fs.to_agent_display(input)
        .ok_or_else(|| anyhow::anyhow!("path is outside your workspace: {input}"))?;
    let host = resolve_host_path(fs, &agent)?;
    Ok((FsTarget::Host(host), agent))
}

/// Points the `path` argument of a physical fs-tool call at the absolute path the
/// on-disk `execute` should act on — the caller's host workspace, or the shuttled
/// copy of a container file — instead of the process working directory.
///
/// The caller's agent-visible path is stashed under [`DISPLAY_PATH_KEY`] so `execute`
/// can show it in its messages — the model must never see the host path. This key is
/// never persisted: tool args are logged from `call.arguments` *before* `run_with`
/// rewrites them, and tool results are plain strings.
pub(crate) fn point_at(agent_path: &str, abs: &Path, mut args: Value) -> Value {
    args[DISPLAY_PATH_KEY] = Value::String(agent_path.to_string());
    args["path"] = Value::String(abs.to_string_lossy().into_owned());
    args
}

// ── Container routing ─────────────────────────────────────────────────────────
//
// The security boundary is the **container**, not the bind-mounted subtree. An
// agent already reaches every corner of its container through `execute_cmd`,
// which runs there with passwordless `sudo`; fs-tools that stopped at the mounts
// were not protecting anything, they were showing a poorer view of the same
// sandbox — and the model routinely answered that by shelling out instead.
//
// So a physical path resolves to one of two backings, and the mount is the *fast*
// one rather than the only one. Host containment is untouched: it is what stops a
// symlink planted in the container from resolving against the **host's** `/etc`,
// and it still guards every path that lands on a mount. The container branch
// never touches the host filesystem, so it has no host to escape from.

/// Where a physical (non-memory) agent path actually lives.
pub enum FsTarget {
    /// A bind-mounted path: host and container see the same bytes, so the tool
    /// acts on the host directly — no `docker exec`, and full media support.
    Host(PathBuf),
    /// A container-only path (`/tmp`, `/etc`, a package's files…), reachable
    /// solely through the container's own filesystem.
    Container { container: String, path: PathBuf },
}

/// Resolves a physical agent path to its backing.
///
/// An absolute path is **container vocabulary** — it is what `execute_cmd` prints
/// and what the agent's shell sees — so it is reverse-mapped first. Landing on a
/// mount takes the host path (`/root/x` *is* `~/x`, which the tools used to
/// reject); landing nowhere means the path exists only inside the container.
pub(crate) fn resolve_target(fs: &UserFs, agent_path: &str) -> Result<FsTarget> {
    if Path::new(agent_path).is_absolute() {
        return match fs.container_to_agent(Path::new(agent_path)) {
            Some(mapped) => Ok(FsTarget::Host(resolve_host_path(fs, &mapped)?)),
            None => Ok(FsTarget::Container {
                container: fs.container_name.clone(),
                path:      lexical_normalize(Path::new(agent_path)),
            }),
        };
    }
    Ok(FsTarget::Host(resolve_host_path(fs, agent_path)?))
}

/// A container file materialised host-side for the duration of one tool call.
///
/// Every single-file fs-tool funnels through the same shape — resolve, then run a
/// sync `execute` that reads and writes one absolute host path. Rather than give
/// each of them a second implementation, with a second set of messages, diffs and
/// edge cases to keep in step, the file is pulled out of the container, the
/// **unchanged** tool runs on the copy, and the copy goes back if it changed.
///
/// A missing remote file is deliberately not pre-created: `write_file` says
/// "Created" or "Overwrote" based on whether the path existed, and a placeholder
/// would make every creation report the wrong one.
pub(crate) struct Shuttle {
    dir:       PathBuf,
    local:     PathBuf,
    container: String,
    remote:    PathBuf,
    /// The bytes as pulled, or `None` when the remote file did not exist.
    /// Compared by content rather than mtime, whose one-second resolution on some
    /// filesystems would miss a fast edit.
    before:    Option<Vec<u8>>,
}

impl Shuttle {
    async fn pull(container: &str, remote: &Path) -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("skald-fs-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await
            .with_context(|| format!("Failed to create temporary directory: {}", dir.display()))?;
        // Keep the basename: tools and media sniffing key off the extension.
        let name = remote.file_name().unwrap_or_else(|| std::ffi::OsStr::new("file"));
        let local = dir.join(name);

        let before = if crate::container::exec_fs::exists(container, remote).await {
            let bytes = crate::container::exec_fs::read(container, remote).await?;
            tokio::fs::write(&local, &bytes).await
                .with_context(|| format!("Failed to stage {}", remote.display()))?;
            Some(bytes)
        } else {
            None
        };

        Ok(Self {
            dir,
            local,
            container: container.to_string(),
            remote:    remote.to_path_buf(),
            before,
        })
    }

    /// Pushes the copy back when the tool created or changed it, then cleans up.
    async fn finish(self) -> Result<()> {
        let after = tokio::fs::read(&self.local).await.ok();
        let changed = match (&self.before, &after) {
            (before, Some(a)) => before.as_ref() != Some(a),
            (_, None)         => false,
        };
        let pushed = if changed {
            crate::container::exec_fs::write(&self.container, &self.remote, after.as_deref().unwrap_or(&[])).await
        } else {
            Ok(())
        };
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
        pushed
    }
}

/// The single entry point a single-file fs-tool uses for a physical path: resolve
/// the backing, then run the tool's own `execute` against it — directly on the
/// host, or on a shuttled copy for a container-only path.
pub(crate) fn run_physical<'a, T>(
    tool:       &'a T,
    fs:         &UserFs,
    agent_path: &str,
    args:       Value,
) -> Box<dyn ToolExecution + 'a>
where
    T: crate::tools::Tool + ?Sized,
{
    match resolve_target(fs, agent_path) {
        Err(e) => error_exec(e.to_string()),
        Ok(FsTarget::Host(host)) => tool.run(point_at(agent_path, &host, args)),
        Ok(FsTarget::Container { container, path }) => {
            let display = agent_path.to_string();
            Box::new(SimpleExecution::new(Box::pin(async move {
                let shuttle = Shuttle::pull(&container, &path).await?;
                let args = point_at(&display, &shuttle.local, args);
                // The tool's own error wins over a push failure: the push is
                // bookkeeping, the tool's message is what the model must read.
                let out = tool.execute_typed(args).await;
                let pushed = shuttle.finish().await;
                match out {
                    Ok(v)  => pushed.map(|()| v),
                    Err(e) => Err(e),
                }
            })))
        }
    }
}

/// Private stash key for the agent-visible path, set by [`rewrite_to_host`] alongside
/// the host path in `path`.
const DISPLAY_PATH_KEY: &str = "__display_path";

/// The path to show in user-facing messages: the agent-visible path stashed by
/// [`rewrite_to_host`] when present, falling back to `path` itself for the
/// context-free legacy path (where `path` was never rewritten and is already the
/// agent path).
pub(crate) fn display_path_arg(args: &Value) -> &str {
    args.get(DISPLAY_PATH_KEY).and_then(Value::as_str)
        .or_else(|| args.get("path").and_then(Value::as_str))
        .unwrap_or("")
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
    registry.register(AppendFile::new(Arc::clone(&shared_pool)));
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

    /// Container routing: a container-absolute path that names a **mount** takes
    /// the host fast path (`/root/x` *is* `~/x` — it used to be rejected as an
    /// escape, because the absolute tail replaced the home base on `join`), while
    /// one that names nothing mounted resolves inside the container.
    #[test]
    fn absolute_paths_route_to_the_mount_or_to_the_container() {
        use core_api::user_fs::SharedMount;

        let root = std::env::temp_dir().join(format!("skald-fstgt-{}", std::process::id()));
        let home = root.join("homes").join("u1");
        let shared = root.join("shared").join("family");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&shared).unwrap();

        let fs = UserFs::new(
            "u1",
            home.clone(),
            "skald-u1",
            PathBuf::from("/root"),
            vec![SharedMount {
                name:      "family".into(),
                host:      shared.clone(),
                container: PathBuf::from("/root/shared/family"),
                can_write: true,
            }],
            vec![],
            None,
        );

        let home_canon = canonicalize_for_policy(&home.to_string_lossy(), Path::new("/"));
        let shared_canon = canonicalize_for_policy(&shared.to_string_lossy(), Path::new("/"));

        let host = |p: &str| match resolve_target(&fs, p).unwrap() {
            FsTarget::Host(h) => h,
            FsTarget::Container { path, .. } => panic!("{p} routed to the container as {path:?}"),
        };
        let container = |p: &str| match resolve_target(&fs, p).unwrap() {
            FsTarget::Container { container, path } => (container, path),
            FsTarget::Host(h) => panic!("{p} routed to the host as {h:?}"),
        };

        // The container spelling of the home and of a shared mount reach the same
        // host files as the agent vocabulary does.
        assert_eq!(host("/root/notes.md"), host("~/notes.md"));
        assert!(path_under(&host("/root/notes.md"), &home_canon));
        assert_eq!(
            host("/root/shared/family/list.md"),
            host("shared/family/list.md")
        );
        assert!(path_under(&host("/root/shared/family/list.md"), &shared_canon));

        // Nothing mounted there → the container's own filesystem.
        let (name, path) = container("/tmp/cv.txt");
        assert_eq!(name, "skald-u1");
        assert_eq!(path, PathBuf::from("/tmp/cv.txt"));
        assert_eq!(container("/etc/os-release").1, PathBuf::from("/etc/os-release"));
        // `..` is collapsed before it can name a parent of anything.
        assert_eq!(container("/tmp/../tmp/x").1, PathBuf::from("/tmp/x"));

        // A shared folder the user does not belong to stays an error — the
        // container spelling must not become a way around membership.
        assert!(resolve_target(&fs, "/root/shared/secret/x.md").is_err());

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

    /// A memory path spelled as if it lived in the home must still reach the note
    /// store — otherwise it would be written to a *physical* `user-memory/` directory
    /// no reader ever looks at.
    #[test]
    fn classify_memory_accepts_home_spellings() {
        for p in ["~/user-memory/x.md", "/root/user-memory/x.md", "./user-memory/x.md"] {
            let m = classify_memory(p).unwrap_or_else(|| panic!("{p} must classify as memory"));
            assert!(matches!(m.scope, MemScope::User));
            assert_eq!(m.rel, "x.md", "{p}");
        }
        assert!(matches!(
            classify_memory("~/shared-memory/casa.md").unwrap().scope,
            MemScope::Shared
        ));

        // the container-home strip needs a real separator, and stops at the home
        assert!(classify_memory("/rootless/user-memory/x.md").is_none());
        assert!(classify_memory("/root/notes/user-memory/x.md").is_none());
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
        let ctx = ToolContext { session_id: 1, user_id: "u_test".into(), pool: Arc::clone(&user), fs: test_fs(), mcp: None };

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
        let ctx = ToolContext { session_id: 1, user_id: "u_test".into(), pool: Arc::clone(&user), fs: test_fs(), mcp: None };

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
        let ctx = ToolContext { session_id: 1, user_id: "u_test".into(), pool: Arc::clone(&user), fs: test_fs(), mcp: None };

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
        let ctx = ToolContext { session_id: 1, user_id: "u1".into(), pool: Arc::clone(&user), fs, mcp: None };
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

    /// Physical-path fs tools must report the **agent-visible** path in every
    /// message they return — never the resolved host path. The agent's virtual
    /// namespace (`~/…`, `shared/…`, `projects/…`) is all it should ever see;
    /// the host workspace location is an internal detail. Regression for the
    /// host-path leak that `rewrite_to_host` introduced into `execute`'s output.
    #[tokio::test]
    async fn physical_fs_tools_show_agent_path_not_host() {
        let (shared, sdir) = store("phys-shared").await;
        let (user,   udir) = store("phys-user").await;

        let root = std::env::temp_dir().join(format!("skald-phys-{}", uuid::Uuid::new_v4()));
        let home = root.join("homes").join("u1");
        std::fs::create_dir_all(&home).unwrap();

        let fs = Arc::new(UserFs::new(
            "u1", home.clone(), "skald-u1", PathBuf::from("/root"), vec![], vec![], None,
        ));
        let ctx = ToolContext { session_id: 1, user_id: "u1".into(), pool: Arc::clone(&user), fs, mcp: None };
        let write = WriteFile::new(Arc::clone(&shared));
        let edit  = EditFile::new(Arc::clone(&shared));
        let grep  = GrepFiles::new();

        // `/homes/u1` only ever appears in the resolved host path, never in the
        // agent namespace — so it is a robust, OS-independent leak detector.
        let leak_marker = "/homes/u1";

        // write_file success → "Created ~/notes.md", never the host home.
        let out = drive(&write, &ctx, json!({"path":"~/notes.md","content":"hello\nworld"}))
            .await.unwrap();
        assert!(out.contains("~/notes.md"), "agent path missing: {out}");
        assert!(!out.contains(leak_marker), "host path leaked into write_file result: {out}");

        // edit_file failure → the error names the agent path, never the host path.
        let err = drive(&edit, &ctx, json!({"path":"~/notes.md","old":"nope","new":"x"}))
            .await.unwrap_err();
        assert!(err.contains("~/notes.md"), "agent path missing from error: {err}");
        assert!(!err.contains(leak_marker), "host path leaked into edit_file error: {err}");

        // grep_files no-match → "in ~/notes.md", never the host path.
        let out = drive(&grep, &ctx, json!({"path":"~/notes.md","pattern":"zzz"}))
            .await.unwrap();
        assert!(out.contains("~/notes.md"), "agent path missing from grep: {out}");
        assert!(!out.contains(leak_marker), "host path leaked into grep result: {out}");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&udir);
        let _ = std::fs::remove_dir_all(&sdir);
    }

    /// `append_file` on a physical path: creates, adds whole lines, and — the
    /// property the tool exists for — never shortens what was already there.
    #[tokio::test]
    async fn append_file_on_disk_creates_and_only_ever_grows() {
        let (shared, sdir) = store("append-shared").await;
        let (user,   udir) = store("append-user").await;

        let root = std::env::temp_dir().join(format!("skald-append-{}", uuid::Uuid::new_v4()));
        let home = root.join("homes").join("u1");
        std::fs::create_dir_all(&home).unwrap();

        let fs = Arc::new(UserFs::new(
            "u1", home.clone(), "skald-u1", PathBuf::from("/root"), vec![], vec![], None,
        ));
        let ctx = ToolContext { session_id: 1, user_id: "u1".into(), pool: Arc::clone(&user), fs, mcp: None };
        let append = AppendFile::new(Arc::clone(&shared));

        // Absent file → created, with the trailing newline supplied for us.
        let out = drive(&append, &ctx, json!({"path":"~/log.md","content":"first"}))
            .await.unwrap();
        assert!(out.contains("~/log.md"), "agent path missing: {out}");
        assert!(!out.contains("/homes/u1"), "host path leaked: {out}");
        assert_eq!(std::fs::read_to_string(home.join("log.md")).unwrap(), "first\n");

        // Second append lands on its own line, first line untouched.
        drive(&append, &ctx, json!({"path":"~/log.md","content":"second\n"})).await.unwrap();
        assert_eq!(std::fs::read_to_string(home.join("log.md")).unwrap(), "first\nsecond\n");

        // A file that does not end in a newline gets a separator, never a splice.
        std::fs::write(home.join("ragged.md"), "no-newline").unwrap();
        drive(&append, &ctx, json!({"path":"~/ragged.md","content":"next"})).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join("ragged.md")).unwrap(),
            "no-newline\nnext\n",
            "append must not glue itself onto an unterminated last line"
        );

        // Containment holds like every other physical fs tool (blueprint §6).
        let err = drive(&append, &ctx, json!({"path":"../escape.md","content":"x"}))
            .await.unwrap_err();
        assert!(!err.is_empty(), "an escaping path must be rejected");
        assert!(!root.join("escape.md").exists(), "append escaped the home");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&udir);
        let _ = std::fs::remove_dir_all(&sdir);
    }

    /// The skills tree end to end, on disk: both scopes resolve, the bare-id alias
    /// resolves only when unambiguous, a collision fails loudly naming both paths,
    /// an invented scope segment is refused with a hint instead of quietly becoming
    /// a file in the home, and containment holds inside a skill exactly as it does
    /// in the home.
    #[cfg(unix)]
    #[test]
    fn skills_tree_routes_and_contains() {
        use core_api::user_fs::SkillMounts;

        let root = std::env::temp_dir().join(format!("skald-skills-{}", std::process::id()));
        let home = root.join("homes").join("u1");
        let skroot = root.join(".skills-root").join("u1");
        let shared = root.join("skills");
        let own = root.join("skills-users").join("u1");
        let _ = std::fs::remove_dir_all(&root);
        for d in [&home, &skroot, &shared, &own] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(skroot.join("README.md"), "signpost").unwrap();
        std::fs::create_dir_all(shared.join("ics-import")).unwrap();
        std::fs::write(shared.join("ics-import").join("SKILL.md"), "shared one").unwrap();
        std::fs::create_dir_all(own.join("spesa")).unwrap();
        std::fs::write(own.join("spesa").join("SKILL.md"), "mine").unwrap();

        let fs = UserFs::new(
            "u1",
            home.clone(),
            "skald-u1",
            PathBuf::from("/root"),
            vec![],
            vec![],
            None,
        )
        .with_skills(SkillMounts {
            root_host:    skroot.clone(),
            shared_host:  shared.clone(),
            own_host:     own.clone(),
            own_username: "daniele".into(),
        });

        let read = |p: &str| std::fs::read_to_string(resolve_host_path(&fs, p).unwrap()).unwrap();

        // Both scopes, spelled fully.
        assert_eq!(read("skills/shared/ics-import/SKILL.md"), "shared one");
        assert_eq!(read("skills/daniele/spesa/SKILL.md"), "mine");
        // The container spelling reaches the same files (reverse-mapped by
        // `resolve_target`, which is what an absolute path goes through).
        let via_container = match resolve_target(&fs, "/root/skills/shared/ics-import/SKILL.md").unwrap() {
            FsTarget::Host(h) => h,
            FsTarget::Container { path, .. } => panic!("mounted skill routed to the container as {path:?}"),
        };
        assert_eq!(std::fs::read_to_string(via_container).unwrap(), "shared one");
        // The signpost is readable rather than being the one path in the tree that fails.
        assert_eq!(read("skills/README.md"), "signpost");

        // The bare-id alias: the shortest spelling, resolving because each id is
        // unique across the two trees.
        assert_eq!(read("skills/ics-import/SKILL.md"), "shared one");
        assert_eq!(read("skills/spesa/SKILL.md"), "mine");

        // Same id in both trees: neither wins, and the error names both full paths.
        std::fs::create_dir_all(own.join("ics-import")).unwrap();
        std::fs::write(own.join("ics-import").join("SKILL.md"), "my fork").unwrap();
        let err = resolve_host_path(&fs, "skills/ics-import/SKILL.md").unwrap_err().to_string();
        assert!(err.contains("skills/shared/ics-import"), "{err}");
        assert!(err.contains("skills/daniele/ics-import"), "{err}");
        // The full paths still work while the alias is ambiguous.
        assert_eq!(read("skills/shared/ics-import/SKILL.md"), "shared one");
        assert_eq!(read("skills/daniele/ics-import/SKILL.md"), "my fork");

        // An invented scope segment: refused with a hint, and — the part that matters
        // — it never becomes a path under the home that no indexer would ever read.
        let err = resolve_host_path(&fs, "skills/pippo/SKILL.md").unwrap_err().to_string();
        assert!(err.contains("other members' skills are not accessible"), "{err}");
        assert!(!home.join("skills").exists(), "the invented scope leaked into the home");

        // Containment inside a skill: a symlink planted in one cannot lead out of it.
        std::os::unix::fs::symlink(&root, shared.join("ics-import").join("escape")).unwrap();
        assert!(resolve_host_path(&fs, "skills/shared/ics-import/escape/homes/u1/x").is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

}
