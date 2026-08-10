//! Read-only access to the git history of workspace files.
//!
//! Project versioning is agent-driven (the project-coordinator commits inside
//! the user's container, straight into the bind-mounted project folder); this
//! module is the *read* side, backing the file viewer's history mode:
//!
//! - [`GitVersions::history`] lists the commits that touched a file;
//! - [`GitVersions::tree_at`] materializes a full copy of the repository at a
//!   revision — `git archive` streamed through the host `tar` — into a
//!   content-addressed cache, and [`GitVersions::file_at`] resolves one file
//!   inside it.
//!
//! Serving a revision from a whole extracted tree (never from the working
//! tree) is what makes dependency-bearing formats correct: a `.tex` compiles
//! against the `\input`s and images *of that revision*, and a markdown file's
//! relative assets load contemporaneously too. Extracted trees are immutable
//! by construction, so the cache needs no invalidation — only a size-bounded
//! oldest-first prune.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio::sync::OnceCell;

/// One commit that touched a file (`%H`, `%aI`, `%s` — see [`parse_history`]).
#[derive(Debug, Clone, Serialize)]
pub struct VersionEntry {
    /// Full commit sha.
    pub rev:     String,
    /// Author date, ISO-8601.
    pub date:    String,
    /// Commit subject line.
    pub subject: String,
}

/// Cache root name for extracted trees, under the OS temp dir.
const TREES_DIR_NAME: &str = "skald-git-trees";
/// Total size ceiling for extracted trees; oldest extractions are pruned.
const TREES_MAX_BYTES: u64 = 1 << 30; // 1 GiB
/// The cache is re-walked for pruning at most this often.
const PRUNE_INTERVAL: Duration = Duration::from_secs(600);
/// Versions listed per file, at most.
const HISTORY_LIMIT: &str = "200";
/// Timeout for one git invocation (log, rev-parse) and for archive+extract.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);

/// Accept only hex shas. Beyond rejecting junk this is what keeps `rev`
/// option-injection-safe when handed to git as an argument: a string starting
/// with `-` can never pass.
pub fn valid_rev(rev: &str) -> bool {
    (7..=64).contains(&rev.len()) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Facade over the host `git` binary plus the extracted-tree cache. Owns only
/// paths and prune state; constructed once and shared via `Arc` (on `Skald`).
pub struct GitVersions {
    trees_dir:  PathBuf,
    git_ok:     OnceCell<bool>,
    last_prune: Mutex<Option<Instant>>,
}

impl Default for GitVersions {
    fn default() -> Self { Self::new() }
}

impl GitVersions {
    pub fn new() -> Self {
        Self {
            trees_dir:  std::env::temp_dir().join(TREES_DIR_NAME),
            git_ok:     OnceCell::new(),
            last_prune: Mutex::new(None),
        }
    }

    /// `git` reachable on the host PATH (memoized). The repos are committed
    /// from inside containers, but they live on host bind mounts and reading
    /// them (`log`, `archive`) needs no identity or write access, so the host
    /// git is sufficient — and may be absent, in which case history mode
    /// simply never appears.
    pub async fn available(&self) -> bool {
        *self
            .git_ok
            .get_or_init(|| async {
                Command::new("git")
                    .arg("--version")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .map(|s| s.success())
                    .unwrap_or(false)
            })
            .await
    }

    /// Walk up from `file` looking for a `.git`, never past `boundary` (the
    /// workspace mount base) — so a dev box's own checkout above the data root
    /// is never mistaken for a user's repo. Returns `(repo_root, rel)`, where
    /// `rel` is `file` relative to the repo root. `.git` may be a directory or
    /// a file (worktrees), hence `.exists()`.
    pub fn repo_for(file: &Path, boundary: &Path) -> Option<(PathBuf, PathBuf)> {
        // Both sides are canonicalized: `boundary` comes from config (lexical)
        // while `file` went through symlink-resolving containment checks, so a
        // symlinked component on either side would otherwise silently disable
        // the boundary — and the walk would escape past the workspace.
        let file = std::fs::canonicalize(file).ok()?;
        let boundary = std::fs::canonicalize(boundary).unwrap_or_else(|_| boundary.to_path_buf());
        let mut dir = file.parent()?;
        loop {
            if dir.join(".git").exists() {
                return Some((dir.to_path_buf(), file.strip_prefix(dir).ok()?.to_path_buf()));
            }
            if dir == boundary || !dir.starts_with(&boundary) {
                return None;
            }
            dir = dir.parent()?;
        }
    }

    /// Commits that touched `rel` in `repo_root`, newest first. `--follow`
    /// keeps the history across renames of the file.
    pub async fn history(&self, repo_root: &Path, rel: &Path) -> Result<Vec<VersionEntry>> {
        let rel = rel.to_string_lossy();
        let out = self
            .git(repo_root, &["log", "--follow", "--format=%H%x1f%aI%x1f%s", "-n", HISTORY_LIMIT, "--", &rel])
            .await?;
        Ok(parse_history(&String::from_utf8_lossy(&out)))
    }

    /// The current HEAD sha, or `None` for a repo with no commits yet (where
    /// `git log` would exit non-zero — the caller treats that as "versioned,
    /// but empty" rather than an error).
    pub async fn head_rev(&self, repo_root: &Path) -> Option<String> {
        let out = self.git(repo_root, &["rev-parse", "--verify", "HEAD"]).await.ok()?;
        let rev = String::from_utf8_lossy(&out).trim().to_string();
        if rev.is_empty() { None } else { Some(rev) }
    }

    /// Materialize the full tree at `rev` into the cache and return its
    /// (canonical) root. Extraction happens once per (repo, revision): the
    /// tar stream is unpacked into a staging dir atomically renamed into
    /// place, so a concurrent request either waits out the race or finds the
    /// finished tree.
    pub async fn tree_at(&self, repo_root: &Path, rev: &str) -> Result<PathBuf> {
        debug_assert!(valid_rev(rev));
        let final_dir = self.trees_dir.join(repo_key(repo_root)).join(rev);
        if final_dir.is_dir() {
            return Ok(tokio::fs::canonicalize(&final_dir).await.unwrap_or(final_dir));
        }

        let staging = final_dir.with_file_name(format!(".{rev}.tmp-{}", unique_suffix()));
        tokio::fs::create_dir_all(&staging).await?;
        if let Err(e) = self.extract_archive(repo_root, rev, &staging).await {
            let _ = tokio::fs::remove_dir_all(&staging).await;
            return Err(e);
        }
        match tokio::fs::rename(&staging, &final_dir).await {
            Ok(()) => {}
            // Lost the race to a concurrent extraction — same content, use it.
            Err(_) if final_dir.is_dir() => {
                let _ = tokio::fs::remove_dir_all(&staging).await;
            }
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&staging).await;
                return Err(e).context("git tree cache rename failed");
            }
        }
        self.maybe_prune();
        Ok(tokio::fs::canonicalize(&final_dir).await.unwrap_or(final_dir))
    }

    /// The on-disk path of `rel` inside the extracted tree at `rev` — `None`
    /// when the file did not exist at that revision. Canonicalize +
    /// prefix-check: a symlink committed inside the repo must not lead reads
    /// out of the tree (the same discipline `resolve_host_path` applies to
    /// the workspace).
    pub async fn file_at(&self, repo_root: &Path, rev: &str, rel: &Path) -> Result<Option<PathBuf>> {
        let tree = self.tree_at(repo_root, rev).await?;
        let candidate = tree.join(rel);
        if !candidate.exists() {
            return Ok(None);
        }
        let canon = tokio::fs::canonicalize(&candidate)
            .await
            .with_context(|| format!("cannot resolve {}", candidate.display()))?;
        if !canon.starts_with(&tree) {
            tracing::warn!(path = %candidate.display(), "git tree entry escapes the tree — refusing");
            return Ok(None);
        }
        Ok(Some(canon))
    }

    /// Run `git -C repo_root <args>`, returning raw stdout. Args are passed as
    /// argv (no shell); stderr text becomes the error on a non-zero exit.
    async fn git(&self, repo_root: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let root = repo_root.to_string_lossy().into_owned();
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&root).args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let out = match tokio::time::timeout(GIT_TIMEOUT, cmd.output()).await {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(e).context("failed to spawn `git`"),
            Err(_) => bail!("git timed out after {GIT_TIMEOUT:?}"),
        };
        if out.status.success() {
            Ok(out.stdout)
        } else {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim())
        }
    }

    /// `git archive <rev>` on stdout, piped into the host `tar` unpacking into
    /// `dest`. git writes the tar itself, so path handling inside the archive
    /// is git's own (always tree-relative); we never interpolate user input
    /// into a command line.
    async fn extract_archive(&self, repo_root: &Path, rev: &str, dest: &Path) -> Result<()> {
        let root = repo_root.to_string_lossy().into_owned();
        let dest_str = dest.to_string_lossy().into_owned();

        let mut git = Command::new("git")
            .arg("-C").arg(&root)
            .args(["archive", "--format=tar", rev])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to spawn `git`")?;
        let mut tar = Command::new("tar")
            .args(["-x", "-C"]).arg(&dest_str)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to spawn `tar`")?;

        let work = async move {
            let mut git_out = git.stdout.take().context("git stdout piped")?;
            let mut tar_in = tar.stdin.take().context("tar stdin piped")?;
            let pump = tokio::io::copy(&mut git_out, &mut tar_in).await;
            drop(tar_in); // EOF, so tar can finish
            let git_outcome = git.wait_with_output().await;
            let tar_outcome = tar.wait_with_output().await;
            // Process errors carry the useful stderr; a bare pump error
            // (broken pipe) is just their symptom, so it is reported last.
            let git_out = git_outcome.context("git wait failed")?;
            if !git_out.status.success() {
                bail!("{}", String::from_utf8_lossy(&git_out.stderr).trim());
            }
            let tar_out = tar_outcome.context("tar wait failed")?;
            if !tar_out.status.success() {
                bail!("tar: {}", String::from_utf8_lossy(&tar_out.stderr).trim());
            }
            pump?;
            Ok(())
        };
        match tokio::time::timeout(GIT_TIMEOUT, work).await {
            Ok(r) => r,
            Err(_) => bail!("git archive timed out after {GIT_TIMEOUT:?}"),
        }
    }

    /// Prune the tree cache if it grew past the ceiling — at most once per
    /// [`PRUNE_INTERVAL`], off the request path. Trees are immutable, so this
    /// is purely a size policy: oldest extraction first.
    fn maybe_prune(&self) {
        {
            let mut last = self.last_prune.lock().unwrap();
            let now = Instant::now();
            if last.is_some_and(|t| now.duration_since(t) < PRUNE_INTERVAL) {
                return;
            }
            *last = Some(now);
        }
        let root = self.trees_dir.clone();
        tokio::task::spawn_blocking(move || prune_trees(&root, TREES_MAX_BYTES));
    }
}

/// Parse `git log --format=%H%x1f%aI%x1f%s` output: one entry per line, fields
/// separated by U+001F. Malformed lines are skipped; entries whose first field
/// is not a sha are dropped (defence in depth — the rev round-trips into later
/// git invocations).
fn parse_history(out: &str) -> Vec<VersionEntry> {
    out.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\u{1f}');
            let rev = fields.next()?.to_string();
            let date = fields.next()?.to_string();
            let subject = fields.next()?.to_string();
            valid_rev(&rev).then_some(VersionEntry { rev, date, subject })
        })
        .collect()
}

/// Cache-dir key for one repository: first 5 bytes of SHA-256 over its
/// canonical path (same convention as the latex cache).
fn repo_key(repo_root: &Path) -> String {
    let key = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let digest = Sha256::digest(key.to_string_lossy().as_bytes());
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Collision-proof suffix for staging dirs: pid + process-wide counter.
fn unique_suffix() -> String {
    format!("{}-{}", std::process::id(), UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Total size of a directory tree, best-effort (unreadable entries count 0).
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let Ok(md) = entry.metadata() else { continue };
            if md.is_dir() {
                total += dir_size(&entry.path());
            } else {
                total += md.len();
            }
        }
    }
    total
}

/// Delete oldest extracted trees (never staging dirs) until the cache fits
/// under `cap`. Runs inside `spawn_blocking`.
fn prune_trees(root: &Path, cap: u64) {
    let mut trees: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    let Ok(repos) = std::fs::read_dir(root) else { return };
    for repo in repos.flatten() {
        let Ok(revs) = std::fs::read_dir(repo.path()) else { continue };
        for rev in revs.flatten() {
            let path = rev.path();
            let Ok(md) = rev.metadata() else { continue };
            if !md.is_dir() || rev.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let size = dir_size(&path);
            total += size;
            trees.push((md.modified().unwrap_or(SystemTime::UNIX_EPOCH), size, path));
        }
    }
    if total <= cap {
        return;
    }
    trees.sort_by_key(|(modified, _, _)| *modified);
    for (_, size, path) in trees {
        if total <= cap {
            break;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run a git command synchronously, skipping the test when git or the
    /// setup fails (CI hosts without git must not fail the suite).
    fn git_sync(root: &Path, args: &[&str]) -> Result<()> {
        let out = std::process::Command::new("git")
            .arg("-C").arg(root)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .context("spawn git")?;
        if out.status.success() {
            Ok(())
        } else {
            bail!("{}", String::from_utf8_lossy(&out.stderr))
        }
    }

    /// A scratch dir under the OS temp dir, unique per test invocation.
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("skald-git-versions-test-{tag}-{}", unique_suffix()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rev_validation() {
        assert!(valid_rev("a1b2c3d"));
        assert!(valid_rev(&"f".repeat(40)));
        assert!(valid_rev(&"9a".repeat(32))); // sha256 repos
        assert!(!valid_rev(""));
        assert!(!valid_rev("HEAD"));
        assert!(!valid_rev("--output=/tmp/x")); // option injection
        assert!(!valid_rev(&"f".repeat(65)));
        assert!(!valid_rev("a1b2c3")); // too short
    }

    #[test]
    fn history_parsing() {
        let out = "a1b2c3d\u{1f}2026-08-03T10:00:00+02:00\u{1f}first commit\n\
                   e4f5a6b\u{1f}2026-08-04T11:30:00+02:00\u{1f}chapter 2: draft\n";
        let entries = parse_history(out);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].rev, "a1b2c3d");
        assert_eq!(entries[1].subject, "chapter 2: draft");
        assert!(parse_history("").is_empty());
        assert!(parse_history("garbage line without separators").is_empty());
    }

    #[test]
    fn repo_discovery_respects_the_boundary() {
        let root = scratch("discovery");
        let repo = root.join("workspace").join("mybook");
        let nested = repo.join("chapters");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let file = nested.join("ch1.tex");
        std::fs::write(&file, "x").unwrap();

        // Found inside the boundary, at the project root.
        let (found, rel) = GitVersions::repo_for(&file, &root.join("workspace")).unwrap();
        assert_eq!(found, std::fs::canonicalize(&repo).unwrap());
        assert_eq!(rel, Path::new("chapters").join("ch1.tex"));

        // Boundary exactly at the repo root still finds it.
        assert!(GitVersions::repo_for(&file, &repo).is_some());

        // Boundary below the repo root: no escape upwards.
        assert!(GitVersions::repo_for(&file, &nested).is_none());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn history_and_tree_extraction_round_trip() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return; // no git on this host
        }
        let root = scratch("roundtrip");
        let repo = root.join("book");
        std::fs::create_dir_all(repo.join("chapters")).unwrap();
        if git_sync(&repo, &["init"]).is_err()
            || git_sync(&repo, &["config", "user.email", "test@example.com"]).is_err()
            || git_sync(&repo, &["config", "user.name", "Test"]).is_err()
        {
            std::fs::remove_dir_all(&root).unwrap();
            return;
        }
        std::fs::write(repo.join("chapters/ch1.tex"), "old chapter").unwrap();
        std::fs::write(repo.join("img.txt"), "old image").unwrap();
        git_sync(&repo, &["add", "-A"]).unwrap();
        git_sync(&repo, &["commit", "-m", "first"]).unwrap();
        std::fs::write(repo.join("chapters/ch1.tex"), "new chapter").unwrap();
        std::fs::write(repo.join("img.txt"), "new image").unwrap();
        git_sync(&repo, &["commit", "-am", "second"]).unwrap();

        let gv = GitVersions::new();
        assert!(gv.available().await);

        let versions = gv.history(&repo, Path::new("chapters/ch1.tex")).await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].subject, "second");
        let head = gv.head_rev(&repo).await.unwrap();
        assert_eq!(head, versions[0].rev);

        // The tree at the first revision holds the old contents — both the
        // file and its "dependency".
        let old = gv.file_at(&repo, &versions[1].rev, Path::new("chapters/ch1.tex")).await.unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(old).unwrap(), "old chapter");
        let old_dep = gv.file_at(&repo, &versions[1].rev, Path::new("img.txt")).await.unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(old_dep).unwrap(), "old image");

        // A file that did not exist at that revision is None, not an error.
        std::fs::write(repo.join("later.txt"), "added later").unwrap();
        git_sync(&repo, &["add", "-A"]).unwrap();
        git_sync(&repo, &["commit", "-m", "third"]).unwrap();
        assert!(gv.file_at(&repo, &versions[1].rev, Path::new("later.txt")).await.unwrap().is_none());

        // Extraction is cached: the second call returns the same canonical dir.
        let t1 = gv.tree_at(&repo, &versions[1].rev).await.unwrap();
        let t2 = gv.tree_at(&repo, &versions[1].rev).await.unwrap();
        assert_eq!(t1, t2);

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(t1).unwrap();
    }
}
