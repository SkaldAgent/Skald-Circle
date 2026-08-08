//! `fetch_repo` — downloads a subtree of a **public git repository** into the
//! caller's workspace (blueprint §7.5).
//!
//! It exists for what a `git clone` does **not** do, and the name says so on
//! purpose: it is shallow, it checks out only the requested subtree, it drops
//! `.git`, it refuses symlinks and oversized downloads before a byte reaches the
//! destination, and it leaves a `.source.json` provenance ticket (URL, sub-path,
//! commit SHA, date) next to the files — the one piece of traceability a clone
//! never writes, without which "where did this come from?" and "did it change
//! upstream?" have no answer later.
//!
//! It deliberately **installs nothing**: files land in `destination`, and if
//! what was downloaded is a skill the installation is a separate
//! `skill_register`, with its own approval card over readable files. A
//! download-and-install in one step would move the human's only review moment
//! onto a card showing *a URL*, and a URL is not reviewable.
//!
//! Network egress stays inside the caller's **container** (`docker exec git …`),
//! never in the Skald process — the sandbox's network identity, not the
//! server's. The sanitization and the move into `destination` run host-side on
//! the bind-mounted staging directory, so the two sides never disagree about
//! what landed.

use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use core_api::user_fs::UserFs;
use serde_json::{Value, json};

use crate::skills::install::{Provenance, SOURCE_FILE, copy_tree};
use crate::tools::fs::resolve_host_path;
use crate::tools::{SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult};

/// Ceiling on how many files a download may carry. Generous for working
/// material (reference trees, examples, configs); far below a bulk mirror,
/// which this tool is not for.
pub const FETCH_MAX_FILES: usize = 2000;

/// Ceiling on a download's total size (64 MiB).
pub const FETCH_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Wall-clock bound on the in-container `git` work. Enforced twice: `timeout`
/// inside the script (so a killed client cannot leave a `git` running in the
/// container) and a Rust-side timeout around the `docker` child as backstop.
const CLONE_TIMEOUT: Duration = Duration::from_secs(300);

/// The limits a fetch enforces, as data so a test can shrink them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    max_files: usize,
    max_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_files: FETCH_MAX_FILES, max_bytes: FETCH_MAX_BYTES }
    }
}

/// What a fetch delivered, for the tool's answer to the model.
#[derive(Debug)]
pub(crate) struct Fetched {
    files:  usize,
    bytes:  u64,
    commit: String,
}

// ── The cloner seam ───────────────────────────────────────────────────────────

/// How a repository gets cloned, as a trait so the tests run the **same script**
/// against a local fixture repo without a Docker daemon.
#[async_trait::async_trait]
pub(crate) trait RepoCloner: Send + Sync {
    /// Clones `url` — only `sub_path`, when given — into a fresh `repo/`
    /// directory under the staging dir, and returns the checked-out commit SHA.
    /// `host_dir` and `container_dir` name the **same** staging directory on the
    /// two sides of the home bind mount; implementations use the side they run
    /// on.
    async fn clone(
        &self,
        url: &str,
        sub_path: Option<&str>,
        host_dir: &Path,
        container_dir: &Path,
    ) -> Result<String>;
}

/// The one clone script, run through `sh -c … _ <url> <sub> <dir> <timeout>`
/// with every value **positional**, so a URL or path containing quotes or
/// `$(…)` is data and not shell syntax — the same rule `exec_fs` follows.
///
/// `--filter=blob:none` keeps a big repository's history and untouched blobs
/// off the wire; not every server supports it, so a filtered clone that fails
/// is retried unfiltered rather than reported. `--sparse` plus
/// `sparse-checkout set` materializes only the requested subtree.
const CLONE_SCRIPT: &str = r#"
set -eu
export GIT_TERMINAL_PROMPT=0
TW=""
if [ "$4" != "0" ]; then TW="timeout $4"; fi
mkdir -p -- "$3"
if [ -z "$2" ]; then
  $TW git clone --quiet --depth 1 "$1" "$3/repo"
else
  if ! $TW git clone --quiet --depth 1 --filter=blob:none --sparse "$1" "$3/repo"; then
    rm -rf -- "$3/repo"
    $TW git clone --quiet --depth 1 --sparse "$1" "$3/repo"
  fi
  git -C "$3/repo" sparse-checkout set "$2"
fi
git -C "$3/repo" rev-parse HEAD
"#;

/// Production cloner: runs [`CLONE_SCRIPT`] **inside the caller's container**
/// via `docker exec`, so the egress keeps the sandbox's network identity.
pub(crate) struct ContainerGit {
    container: String,
}

#[async_trait::async_trait]
impl RepoCloner for ContainerGit {
    async fn clone(
        &self,
        url: &str,
        sub_path: Option<&str>,
        _host_dir: &Path,
        container_dir: &Path,
    ) -> Result<String> {
        let dir = container_dir.to_string_lossy().into_owned();
        let out = run_positional(
            "docker",
            &[
                "exec".into(),
                self.container.clone(),
                "sh".into(),
                "-c".into(),
                CLONE_SCRIPT.into(),
                "_".into(),
                url.into(),
                sub_path.unwrap_or("").into(),
                dir,
                CLONE_TIMEOUT.as_secs().saturating_sub(20).to_string(),
            ],
            CLONE_TIMEOUT,
        )
        .await?;
        parse_sha(&out)
    }
}

/// Runs `program argv…` capturing stdout, with `kill_on_drop` plus a hard
/// timeout. On timeout the dropped child is killed — and for the production
/// cloner the script's own `timeout` is what stops the `git` the dead client
/// would otherwise leave behind in the container.
async fn run_positional(program: &str, argv: &[String], timeout: Duration) -> Result<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn `{program}`"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}s", timeout.as_secs()))?
        .with_context(|| format!("`{program}` failed to report"))?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The clone script's last stdout line is the `rev-parse HEAD` answer.
fn parse_sha(stdout: &str) -> Result<String> {
    stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("the clone produced no commit id"))
}

// ── The fetch itself ──────────────────────────────────────────────────────────

/// Drops the staging directory however the fetch ends — mid-copy failures
/// included — so a refused or interrupted download leaves no litter in the
/// caller's home.
struct Staging(PathBuf);

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        // Take the `.skald` parent too when nothing else is using it — a
        // refused download must leave no litter, and `remove_dir` on a
        // non-empty directory is the cheap no-op that says so.
        if let Some(parent) = self.0.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

/// Downloads `url` (only `sub_path`, when given) into the agent path
/// `destination`, sanitizes host-side, and writes the provenance ticket.
pub(crate) async fn fetch(
    fs: &UserFs,
    url: &str,
    sub_path: Option<&str>,
    destination: &str,
    cloner: &dyn RepoCloner,
    limits: Limits,
) -> Result<Fetched> {
    // The root spellings converge here, not only in the tool's argument check:
    // `Some(".")` reaching the script would take the *sparse* arm and
    // `sparse-checkout set .` would materialize the root files and nothing else.
    let sub_path = sub_path.and_then(|s| {
        let s = s.trim();
        if s.is_empty() || s == "." { None } else { Some(s) }
    });

    // An absolute spelling is container vocabulary: map it back to an agent
    // path first, so the writability check below speaks the same language as
    // the relative case. Landing nowhere means container-only, and a download
    // there could never be reviewed or registered from the host side.
    let agent = if Path::new(destination).is_absolute() {
        match fs.container_to_agent(Path::new(destination)) {
            Some(mapped) => mapped,
            None => bail!(
                "`{destination}` exists only inside your container. Download somewhere you \
                 can reach from both sides — your home (`~/…`), a project or a shared folder."
            ),
        }
    } else {
        destination.to_string()
    };

    if !fs.can_write_to(&agent) {
        bail!(
            "`{destination}` is read-only for you (everything under `skills/` always is — a \
             skill is *installed* with skill_register, never downloaded into place). Pick a \
             folder in your home, or a project/shared folder you can write to."
        );
    }
    let host_dest = resolve_host_path(fs, &agent)?;

    if host_dest.exists() {
        if !host_dest.is_dir() {
            bail!("`{destination}` already exists and is not a folder. Pick a new path.");
        }
        if std::fs::read_dir(&host_dest)
            .with_context(|| format!("cannot read {}", host_dest.display()))?
            .next()
            .is_some()
        {
            bail!(
                "`{destination}` already exists and is not empty — fetch_repo never merges \
                 into files that are already there. Pick a new folder, or empty that one."
            );
        }
    }

    // Staging lives inside the home bind mount: the container's `git` writes
    // there, and the host-side half then validates and moves from the same
    // bytes. `.skald` is transient state, hidden from the agent's listings.
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let stage_host = fs.home_host.join(".skald").join(format!("fetch-{tag}"));
    let stage_container = fs.container_home.join(".skald").join(format!("fetch-{tag}"));
    std::fs::create_dir_all(&stage_host)
        .with_context(|| format!("cannot stage in {}", stage_host.display()))?;
    let _staging = Staging(stage_host.clone());

    let commit = cloner
        .clone(url, sub_path, &stage_host, &stage_container)
        .await?;

    // `.git` never crosses into the destination. For a subtree fetch it could
    // not travel anyway; for a root fetch this removal is the guarantee, so it
    // runs in both cases rather than being the subtree case's good fortune.
    let dotgit = stage_host.join("repo").join(".git");
    if dotgit.is_dir() {
        std::fs::remove_dir_all(&dotgit).context("cannot drop the `.git` history")?;
    } else if dotgit.exists() {
        std::fs::remove_file(&dotgit).context("cannot drop the `.git` history")?;
    }

    let extracted = match sub_path {
        None => stage_host.join("repo"),
        Some(sub) => stage_host.join("repo").join(sub),
    };
    if !extracted.is_dir() {
        match sub_path {
            Some(sub) => bail!("the repository has no `{sub}` folder."),
            None => bail!("the clone produced no files."),
        }
    }

    let mut found = Sanitized::default();
    sanitize(&extracted, Path::new(""), &limits, &mut found)?;

    std::fs::create_dir_all(&host_dest)
        .with_context(|| format!("cannot create {destination}"))?;
    copy_tree(&extracted, &host_dest)?;

    let ticket = Provenance {
        url:          url.to_string(),
        sub_path:     sub_path.map(str::to_string),
        commit:       Some(commit.clone()),
        fetched_at:   Some(chrono::Utc::now().to_rfc3339()),
        installed_at: None,
    };
    let json = serde_json::to_string_pretty(&ticket)?;
    std::fs::write(host_dest.join(SOURCE_FILE), json)
        .with_context(|| format!("cannot write the {SOURCE_FILE} ticket"))?;

    Ok(Fetched { files: found.files, bytes: found.bytes, commit })
}

/// Recursive walk that enforces the download rules while it counts. Every
/// refusal names what to fix — the caller is a model that will retry, and
/// "download rejected" would only produce a guess.
#[derive(Default)]
struct Sanitized {
    files: usize,
    bytes: u64,
}

fn sanitize(dir: &Path, rel: &Path, limits: &Limits, acc: &mut Sanitized) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let name = entry.file_name();
        let child_rel = rel.join(&name);
        // `symlink_metadata` does NOT follow the link, which is the point: a
        // link is refused for what it is, before anything asks where it points.
        let meta = std::fs::symlink_metadata(entry.path())
            .map_err(|e| anyhow::anyhow!("cannot stat {}: {e}", child_rel.display()))?;

        if meta.file_type().is_symlink() {
            bail!(
                "the repository contains a symbolic link (`{}`). fetch_repo does not bring \
                 links over — download the real file instead.",
                child_rel.display()
            );
        }
        if meta.is_dir() {
            sanitize(&entry.path(), &child_rel, limits, acc)?;
            continue;
        }
        if !meta.is_file() {
            bail!("`{}` is not a regular file.", child_rel.display());
        }

        acc.files += 1;
        acc.bytes += meta.len();
        if acc.files > limits.max_files {
            bail!(
                "that subtree holds more than {} files — fetch_repo is for working material, \
                 not bulk mirrors.",
                limits.max_files
            );
        }
        if acc.bytes > limits.max_bytes {
            bail!(
                "that subtree is over {} MiB — fetch_repo is for working material, not bulk \
                 data. Clone it by hand with execute_cmd if you really need it all.",
                limits.max_bytes / (1024 * 1024)
            );
        }
    }
    Ok(())
}

// ── Argument parsing ──────────────────────────────────────────────────────────

/// Public repositories over https only: ssh would need credentials the caller
/// does not have (and should not), and a local path is not a repository fetch.
fn check_url(url: &str) -> Result<()> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(())
    } else {
        bail!(
            "fetch_repo downloads from public repositories over https: `{url}`. (No ssh or \
             local paths — no credentials are involved, and none should be.)"
        )
    }
}

/// Normalizes the `sub_path` argument: `""` / `"."` mean the repository root,
/// anything else must be a relative path that stays inside the repo.
fn check_sub_path(raw: &str) -> Result<Option<String>> {
    let s = raw.trim();
    if s.is_empty() || s == "." {
        return Ok(None);
    }
    let p = Path::new(s);
    if p.is_absolute() {
        bail!("`sub_path` is relative to the repository root, not absolute: `{s}`");
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        bail!("`sub_path` must stay inside the repository: `{s}`");
    }
    let normalized: PathBuf = p
        .components()
        .filter_map(|c| match c {
            Component::Normal(x) => Some(x),
            _ => None,
        })
        .collect();
    if normalized.as_os_str().is_empty() {
        return Ok(None);
    }
    Ok(Some(normalized.to_string_lossy().replace('\\', "/")))
}

// ── The tool ──────────────────────────────────────────────────────────────────

pub struct FetchRepo;

impl Tool for FetchRepo {
    fn name(&self) -> &str { crate::tools::tool_names::FETCH_REPO }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Config }
    fn display_name(&self) -> &str { "Fetch Repository" }

    fn description(&self) -> &str {
        "Download a subtree of a public git repository (https only) into a folder you can \
         write — your home, a project or a shared folder. Shallow and sanitized: no `.git` \
         history, no symbolic links, size limits apply. It installs NOTHING: the files are \
         left at `destination`, plus a `.source.json` ticket recording the URL, sub-path and \
         exact commit. If the download is a skill, review the files and then install it with \
         `skill_register` — never download straight into `skills/`, which is read-only. \
         `destination` must not exist yet (or be an empty folder); a path that exists only \
         inside the container, such as /tmp, is refused."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["url", "destination"],
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The repository's https URL, e.g. \
                                    \"https://github.com/anthropics/skills\"."
                },
                "sub_path": {
                    "type": "string",
                    "description": "Folder inside the repository to download, e.g. \
                                    \"skills/ics-import\". Omit, or pass \"\" or \".\", \
                                    to take the whole repository."
                },
                "destination": {
                    "type": "string",
                    "description": "Agent path of the folder to fill, e.g. \
                                    \"~/downloads/ics-import\". Created if missing; must be \
                                    empty if it already exists."
                }
            }
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let url = args["url"].as_str().unwrap_or("?");
        let dest = args["destination"].as_str().unwrap_or("?");
        match args["sub_path"].as_str().filter(|s| !s.is_empty() && *s != ".") {
            Some(sub) => format!("download `{sub}` of {url} into `{dest}`"),
            None => format!("download {url} into `{dest}`"),
        }
    }

    fn target_path(&self, args: &Value) -> Option<String> {
        args["destination"].as_str().map(str::to_string)
    }

    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        let fs = Arc::clone(&ctx.fs);
        Box::new(SimpleExecution::new(Box::pin(async move {
            let url = args["url"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing required argument `url`"))?;
            check_url(url)?;
            let sub = match args["sub_path"].as_str() {
                Some(raw) => check_sub_path(raw)?,
                None => None,
            };
            let destination = args["destination"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing required argument `destination`"))?;

            let cloner = ContainerGit { container: fs.container_name.clone() };
            let done = fetch(&fs, url, sub.as_deref(), destination, &cloner, Limits::default()).await?;

            let short = done.commit.chars().take(7).collect::<String>();
            Ok(ToolResult::Text(format!(
                "Fetched {} files ({:.0} KiB) from {url} @ {short} into {destination}.\n\
                 A `{SOURCE_FILE}` next to the files records where they came from.\n\
                 Nothing was installed — if this is a skill, review the files and then call \
                 `skill_register` on `{destination}`.",
                done.files,
                done.bytes as f64 / 1024.0,
            )))
        }        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::tests_support::Tree;

    // ── Fixture: a local git repository, cloned by the same script ────────────

    /// The test cloner runs the **same** [`CLONE_SCRIPT`] on the host (the dev
    /// machine has git; `$4 = 0` selects the no-`timeout` arm, since macOS
    /// lacks the GNU binary). Only the Docker wrapper is faked away.
    struct HostGit;

    #[async_trait::async_trait]
    impl RepoCloner for HostGit {
        async fn clone(
            &self,
            url: &str,
            sub_path: Option<&str>,
            host_dir: &Path,
            _container_dir: &Path,
        ) -> Result<String> {
            let out = run_positional(
                "sh",
                &[
                    "-c".into(),
                    CLONE_SCRIPT.into(),
                    "_".into(),
                    url.into(),
                    sub_path.unwrap_or("").into(),
                    host_dir.to_string_lossy().into_owned(),
                    "0".into(),
                ],
                Duration::from_secs(60),
            )
            .await?;
            parse_sha(&out)
        }
    }

    struct Repo(PathBuf);

    impl Repo {
        /// A fixture repository with two top-level folders and a root file.
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "skald-fetchrepo-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("alpha")).unwrap();
            std::fs::create_dir_all(dir.join("beta/nested")).unwrap();
            std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
            std::fs::write(dir.join("alpha/a.txt"), "alpha\n").unwrap();
            std::fs::write(dir.join("alpha/second.txt"), "second\n").unwrap();
            std::fs::write(dir.join("beta/b.txt"), "beta\n").unwrap();
            std::fs::write(dir.join("beta/nested/deep.txt"), "deep\n").unwrap();
            let r = Repo(dir);
            r.git(&["init", "-q"]);
            r.git(&["add", "-A"]);
            r.git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]);
            r
        }

        fn git(&self, args: &[&str]) -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&self.0)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn head(&self) -> String {
            self.git(&["rev-parse", "HEAD"])
        }

        fn write(&self, rel: &str, body: &str) {
            std::fs::write(self.0.join(rel), body).unwrap();
        }

        fn commit_all(&self) {
            self.git(&["add", "-A"]);
            self.git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "more"]);
        }
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn fetch_with(fs: &UserFs, url: &str, sub: Option<&str>, dest: &str) -> Result<Fetched> {
        fetch(fs, url, sub, dest, &HostGit, Limits::default()).await
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p.clone());
                }
                out.push(p.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/"));
            }
        }
        out.sort();
        out
    }

    // ── The fetch ─────────────────────────────────────────────────────────────

    /// Only the requested subtree lands in the destination — the rest of the
    /// repository never crosses over.
    #[tokio::test]
    async fn only_the_requested_subtree_lands_in_the_destination() {
        let repo = Repo::new("sub");
        let t = Tree::new("sub", "daniele");

        fetch_with(&t.fs, &repo.0.to_string_lossy(), Some("alpha"), "~/dl")
            .await
            .unwrap();

        let files = listing(&t.root.join("homes/u1/dl"));
        assert!(files.contains(&"a.txt".to_string()), "{files:?}");
        assert!(files.contains(&"second.txt".to_string()), "{files:?}");
        assert!(!files.iter().any(|f| f.contains("beta") || f.contains("README")), "{files:?}");
    }

    /// A root fetch gets everything but the `.git` history — the one thing the
    /// tool exists to keep out.
    #[tokio::test]
    async fn a_root_fetch_gets_everything_but_the_git_history() {
        let repo = Repo::new("root");
        let t = Tree::new("root", "daniele");

        for sub in [None, Some(""), Some(".")] {
            let dest = format!("~/dl-{}", sub.unwrap_or("bare").replace('.', "dot"));
            fetch_with(&t.fs, &repo.0.to_string_lossy(), sub, &dest)
                .await
                .unwrap_or_else(|e| panic!("sub {sub:?}: {e}"));
            let host = t.root.join("homes/u1").join(&dest[2..]);
            let files = listing(&host);
            assert!(files.iter().any(|f| f == "beta/nested/deep.txt"), "{files:?}");
            assert!(!files.iter().any(|f| f.contains(".git")), "{files:?}");
        }
    }

    /// The ticket records the exact commit — the field that makes a later
    /// upstream change detectable.
    #[tokio::test]
    async fn the_provenance_ticket_carries_the_commit() {
        let repo = Repo::new("prov");
        let t = Tree::new("prov", "daniele");

        fetch_with(&t.fs, &repo.0.to_string_lossy(), Some("alpha"), "~/dl")
            .await
            .unwrap();

        let raw = std::fs::read_to_string(t.root.join("homes/u1/dl/.source.json")).unwrap();
        let p: Provenance = serde_json::from_str(&raw).unwrap();
        assert_eq!(p.url, repo.0.to_string_lossy());
        assert_eq!(p.sub_path.as_deref(), Some("alpha"));
        assert_eq!(p.commit.as_deref(), Some(repo.head().as_str()));
        assert!(p.fetched_at.is_some());
        assert!(p.installed_at.is_none(), "install stamps that one");
    }

    /// A `sub_path` the repository does not have is a speaking refusal, not an
    /// empty destination.
    #[tokio::test]
    async fn a_missing_sub_path_is_refused() {
        let repo = Repo::new("nosub");
        let t = Tree::new("nosub", "daniele");

        let e = fetch_with(&t.fs, &repo.0.to_string_lossy(), Some("gamma"), "~/dl")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("no `gamma` folder"), "{e}");
        assert!(!t.root.join("homes/u1/dl").exists(), "nothing landed");
        assert!(!t.root.join("homes/u1/.skald").exists(), "no staging litter");
    }

    /// A symlink is refused for being one, wherever it points — and the
    /// destination stays untouched.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_in_the_repo_is_refused() {
        let repo = Repo::new("symlink");
        std::os::unix::fs::symlink("/etc/passwd", repo.0.join("alpha/secrets")).unwrap();
        repo.commit_all();
        let t = Tree::new("symlink", "daniele");

        let e = fetch_with(&t.fs, &repo.0.to_string_lossy(), Some("alpha"), "~/dl")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("symbolic link"), "{e}");
        assert!(!t.root.join("homes/u1/dl").exists(), "nothing landed");
    }

    /// Over the file cap the refusal comes **before** a byte is written to the
    /// destination (the cap here is the test's, not the shipped one).
    #[tokio::test]
    async fn over_the_file_cap_is_refused_before_writing() {
        let repo = Repo::new("cap");
        let t = Tree::new("cap", "daniele");
        let limits = Limits { max_files: 2, max_bytes: FETCH_MAX_BYTES };

        let e = fetch(&t.fs, &repo.0.to_string_lossy(), None, "~/dl", &HostGit, limits)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("more than 2 files"), "{e}");
        assert!(!t.root.join("homes/u1/dl").exists(), "nothing landed");
    }

    // ── The destination rules ─────────────────────────────────────────────────

    /// `skills/` is read-only in both directions: a download is never the way
    /// in — `skill_register` is.
    #[tokio::test]
    async fn the_skills_tree_is_not_a_destination() {
        let repo = Repo::new("ro");
        let t = Tree::new("ro", "daniele");

        let e = fetch_with(&t.fs, &repo.0.to_string_lossy(), None, "skills/shared/x")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("read-only"), "{e}");
    }

    /// A container-only path is refused with a message that says where to go —
    /// the download would be unreachable from the host half.
    #[tokio::test]
    async fn a_container_only_destination_is_refused() {
        let repo = Repo::new("conly");
        let t = Tree::new("conly", "daniele");

        let e = fetch_with(&t.fs, &repo.0.to_string_lossy(), None, "/tmp/dl")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("only inside your container"), "{e}");
    }

    /// Existing is fine only while it is empty; a non-empty folder is never
    /// merged into.
    #[tokio::test]
    async fn an_existing_destination_must_be_empty() {
        let repo = Repo::new("exists");
        let t = Tree::new("exists", "daniele");

        let home = t.root.join("homes/u1");
        std::fs::create_dir_all(home.join("empty")).unwrap();
        std::fs::create_dir_all(home.join("full")).unwrap();
        std::fs::write(home.join("full/keep.txt"), "mine\n").unwrap();
        std::fs::write(home.join("afile"), "file\n").unwrap();

        fetch_with(&t.fs, &repo.0.to_string_lossy(), Some("alpha"), "~/empty")
            .await
            .unwrap();

        let e = fetch_with(&t.fs, &repo.0.to_string_lossy(), None, "~/full")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not empty"), "{e}");

        let e = fetch_with(&t.fs, &repo.0.to_string_lossy(), None, "~/afile")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("not a folder"), "{e}");
    }

    /// The absolute spelling of a mounted path is the same destination — the
    /// container vocabulary maps back before any check runs.
    #[tokio::test]
    async fn an_absolute_home_spelling_works() {
        let repo = Repo::new("abs");
        let t = Tree::new("abs", "daniele");

        fetch_with(&t.fs, &repo.0.to_string_lossy(), Some("alpha"), "/root/dl")
            .await
            .unwrap();
        assert!(t.root.join("homes/u1/dl/a.txt").exists());
    }

    // ── Argument parsing ──────────────────────────────────────────────────────

    #[test]
    fn only_public_https_urls_pass() {
        assert!(check_url("https://github.com/x/y").is_ok());
        assert!(check_url("http://example.com/r.git").is_ok());
        for bad in ["git@github.com:x/y.git", "ssh://git@h/r", "file:///tmp/r", "/tmp/r"] {
            assert!(check_url(bad).is_err(), "accepted `{bad}`");
        }
    }

    #[test]
    fn the_root_spellings_mean_no_subtree() {
        assert_eq!(check_sub_path("").unwrap(), None);
        assert_eq!(check_sub_path(".").unwrap(), None);
        assert_eq!(check_sub_path("a/b").unwrap(), Some("a/b".to_string()));
        assert_eq!(check_sub_path("./a/./b").unwrap(), Some("a/b".to_string()));
        assert!(check_sub_path("/etc").is_err());
        assert!(check_sub_path("../out").is_err());
        assert!(check_sub_path("a/../../out").is_err());
    }

    // ── The crossing into an installation ─────────────────────────────────────

    /// What `fetch_repo` leaves behind is what `skill_register` picks up: the
    /// ticket crosses into the installed skill, stamped with the install date.
    #[tokio::test]
    async fn a_fetched_skill_registers_with_its_provenance() {
        let repo = Repo::new("cross");
        repo.write("alpha/SKILL.md", &crate::skills::tests_support::valid("alpha-x", "The x."));
        repo.commit_all();
        let t = Tree::new("cross", "daniele");

        fetch_with(&t.fs, &repo.0.to_string_lossy(), Some("alpha"), "~/draft")
            .await
            .unwrap();

        let host = t.root.join("homes/u1/draft");
        let done = crate::skills::install::install(&t.fs, crate::skills::Scope::Own, &host).unwrap();
        assert_eq!(done.id, "alpha-x");

        let p = crate::skills::install::read_provenance(
            &t.root.join("skills-users/u1/alpha-x"),
        )
        .unwrap();
        assert_eq!(p.commit.as_deref(), Some(repo.head().as_str()));
        assert!(p.installed_at.is_some(), "the install stamped its date");
    }
}
