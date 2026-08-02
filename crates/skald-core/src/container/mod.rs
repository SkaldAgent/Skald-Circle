//! Per-user Docker containers (blueprint §6): the execution sandbox.
//!
//! Each user gets one **permanent** container (`skald-{userid}`), built from our
//! own image (`skald-runtime`, python + node). The container is created when the
//! user is created and started at application boot; `execute_cmd` and — later —
//! the user's stateful MCP servers run inside it, against the user's bind-mounted
//! home (`{WD}/homes/{userid}` → `/root`) plus the shared folders they belong to,
//! plus the read-only `{WD}/docs` bundle mounted at `/root/docs` for every user and
//! the read-only memory **signposts** at `/root/{user,shared}-memory` (see
//! [`signpost_mounts`]).
//!
//! Docker is a **hard requirement**: [`ContainerManager::check_docker`] fails
//! construction if the daemon is unreachable, and the shell exits at boot.
//!
//! We shell out to the `docker` CLI rather than link a Docker client crate: fewer
//! dependencies, and the same process-spawning shape `execute_cmd` already uses.
//! The container holds no durable state — everything lives in the bind mounts — so
//! a container can be recreated from the image at any time; boot reconciliation
//! relies on that.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sqlx::SqlitePool;

use core_api::user_fs::{ProjectMount, SharedMount, UserFs};

use crate::db;
use crate::tools::fs as fs_tools;

/// Our runtime image tag. Built once from the embedded [`Dockerfile`]. The version
/// suffix is the image cache-buster: [`ContainerManager::ensure_image`] rebuilds only
/// when the tag is absent, so **bump it whenever the [`Dockerfile`] changes** (`v2`
/// added `sudo` + a NOPASSWD sudoers for the non-root container user; `v3` added
/// `unzip` + `ffmpeg`). Old tags linger as orphaned images (harmless), but existing
/// containers still *run* one — which is why [`reusable`] also compares the image.
const IMAGE_TAG: &str = "skald-runtime:v3";

/// The embedded Dockerfile — the source of truth, so the image can be built with
/// no files shipped alongside the binary (binary-first).
const DOCKERFILE: &str = include_str!("Dockerfile");

/// Subdirectory of the working directory holding per-user homes.
pub const HOMES_DIR: &str = "homes";
/// Subdirectory of the working directory holding shared folders.
pub const SHARED_DIR: &str = "shared";
/// Subdirectory of the working directory holding project folders
/// (`{WD}/projects/{owner_userid}/{slug}`).
pub const PROJECTS_DIR: &str = "projects";
/// Subdirectory of the working directory holding the docs bundle, mounted
/// read-only into every user's container at `{container_home}/docs`.
pub const DOCS_DIR: &str = "docs";
/// Subdirectory of the working directory holding the memory **signposts** — see
/// [`signpost_mounts`]. Dot-prefixed: it is internal plumbing, not a user folder.
pub const SIGNPOST_DIR: &str = ".memory-signpost";
/// Home mount point inside the container.
pub const CONTAINER_HOME: &str = "/root";
/// Grace window `docker stop` gives in-container processes (SIGTERM → SIGKILL)
/// before force-killing — enough for a shell or MCP `docker exec` child to exit.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Grace window at **app shutdown** ([`ContainerManager::stop_all`]). Deliberately
/// short: a healthy container (tini as PID 1, via `--init`) exits within ~100 ms of
/// SIGTERM, so this only bounds the pathological case — an old container whose PID 1
/// is `sleep infinity` (created before `--init`) ignores SIGTERM entirely (the kernel
/// applies no default signal disposition to PID 1) and would otherwise burn the full
/// 10 s `STOP_GRACE` before SIGKILL, once **per user, in sequence**. `ensure`'s init
/// self-heal recreates such containers with tini on the next boot; this cap keeps the
/// shutdown fast in the meantime.
const SHUTDOWN_STOP_GRACE: Duration = Duration::from_secs(2);

/// The deterministic container name for a user — derivable without any manager,
/// so `UserFs` can carry it and `execute_cmd` can exec into it directly.
pub fn container_name(user_id: &str) -> String {
    format!("skald-{user_id}")
}

/// The host process's own `(uid, gid)`, or `None` on non-unix. We run each container
/// as this uid:gid (blueprint §6 UID coherence) so files created inside the container
/// and by the host-side fs-tools share ownership on the bind mounts. On non-unix we
/// fall back to the image default (root) and skip `--user`.
#[cfg(unix)]
fn host_uid_gid() -> Option<(u32, u32)> {
    Some((unsafe { libc::getuid() }, unsafe { libc::getgid() }))
}
#[cfg(not(unix))]
fn host_uid_gid() -> Option<(u32, u32)> {
    None
}

/// Builds the [`UserFs`] view for a user: private home + the shared folders they
/// belong to, plus the container those mount into. Host paths are absolute
/// (anchored at the process working directory), as Docker bind mounts require.
pub async fn build_user_fs(system: &SqlitePool, user_id: &str) -> Result<UserFs> {
    let wd = std::env::current_dir().context("failed to read working directory")?;
    let home_host = wd.join(HOMES_DIR).join(user_id);
    let container_home = PathBuf::from(CONTAINER_HOME);

    let memberships = db::shared_folders::list_for_user(system, user_id).await?;
    let shared = memberships
        .into_iter()
        .map(|m| SharedMount {
            container: container_home.join(SHARED_DIR).join(&m.folder_name),
            host:      wd.join(SHARED_DIR).join(&m.folder_name),
            name:      m.folder_name,
            can_write: m.can_write,
        })
        .collect();

    // Projects (owned + shared-with-them). Host keys on the owner's stable userid; the
    // agent/container path keys on the owner's username (`projects/{owner_username}/{slug}`).
    let project_rows = db::project_members::list_for_user_mounts(system, user_id).await?;
    let projects = project_rows
        .into_iter()
        .map(|p| ProjectMount {
            container: container_home
                .join(PROJECTS_DIR)
                .join(&p.owner_username)
                .join(&p.slug),
            host: wd.join(PROJECTS_DIR).join(&p.owner_user_id).join(&p.slug),
            owner_username: p.owner_username,
            slug: p.slug,
            can_write: p.can_write,
        })
        .collect();

    let docs_host = Some(wd.join(DOCS_DIR));

    Ok(UserFs::new(user_id, home_host, container_name(user_id), container_home, shared, projects, docs_host))
}

// ── Memory signposts ──────────────────────────────────────────────────────────
//
// `user-memory/` and `shared-memory/` are **virtual**: the fs-tools classify those
// prefixes and route them to SQLite (`memory_docs`), so nothing of them exists on
// disk. Inside the container that used to mean bash saw nothing at all — and the
// nothing was worse than it sounds. `cat user-memory/x.md` returned a bare ENOENT,
// which tells a model that the note is missing rather than that it used the wrong
// door; and `mkdir -p user-memory && echo … > user-memory/x.md` *succeeded*,
// writing a real file into the home that no reader ever visits (every reader —
// `read_file`, `list_files`, `memory_search`, the lints, the viewer — goes to
// `memory_docs`), which the next `ls` then confirms as if it had worked.
//
// So each root gets a **read-only bind mount** carrying a README that names the
// tools to use instead. Two deliberate choices:
//
// - *Read-only as a mount, not as a mode.* The container user holds passwordless
//   `sudo`, so a `chmod 0555` would be a suggestion; a `:ro` bind mount holds,
//   because remounting it needs `CAP_SYS_ADMIN` and the container has none. Writes
//   fail with EROFS.
// - *A README rather than an empty directory.* `Permission denied` is an error, not
//   an instruction — models answer it by reaching for `sudo`. The README puts the
//   correction in the same directory the failing command just named, which is the
//   one feedback channel that lands in the turn where the mistake happened.
//
// These mounts are **not** part of [`UserFs`]: they back no agent path (the agent
// path `user-memory/…` is the note store) and the host-side fs-tools must never
// resolve into them. They exist only inside the sandbox, which is the only place
// the confusion happens.

const SIGNPOST_README: &str = "README.md";

/// The signpost text for `user-memory/`. Addressed to the agent, in the vocabulary
/// its tools use.
const USER_MEMORY_SIGNPOST: &str = "\
# This is not a folder

`user-memory/` is a **virtual note store**, kept in the database, not on disk. This
directory is a signpost and is read-only: shell commands cannot read or write your
memory, and anything you manage to write near here is lost.

Use the tools instead — they take the same paths:

    read_file    path=\"user-memory/notes/x.md\"
    write_file   path=\"user-memory/notes/x.md\"  content=\"…\"
    edit_file    path=\"user-memory/notes/x.md\"  …
    list_files   path=\"user-memory/\"
    memory_search query=\"<keywords>\"

`grep_files` does not reach the store either — use `memory_search`.
";

/// The signpost text for `shared-memory/`. Same rule; the extra line is the one
/// thing that differs about the shared store.
const SHARED_MEMORY_SIGNPOST: &str = "\
# This is not a folder

`shared-memory/` is a **virtual note store** shared with the whole group, kept in the
database, not on disk. This directory is a signpost and is read-only: shell commands
cannot read or write it, and anything you manage to write near here is lost.

Use the tools instead — they take the same paths:

    read_file    path=\"shared-memory/x.md\"
    write_file   path=\"shared-memory/x.md\"  content=\"…\"
    edit_file    path=\"shared-memory/x.md\"  …
    list_files   path=\"shared-memory/\"
    memory_search query=\"<keywords>\"

Writing here asks the user to confirm first — that is expected, not an error.
`grep_files` does not reach the store either — use `memory_search`.
";

/// Where the two signposts live on the host and where they mount, read-only, in the
/// container. One pair of host directories for the whole instance: the content is
/// identical for every user, and the mount is a sign, not a workspace.
fn signpost_mounts(wd: &Path, container_home: &Path) -> [(PathBuf, PathBuf); 2] {
    let root = wd.join(SIGNPOST_DIR);
    [
        (
            root.join(fs_tools::USER_MEMORY_ROOT),
            container_home.join(fs_tools::USER_MEMORY_ROOT),
        ),
        (
            root.join(fs_tools::SHARED_MEMORY_ROOT),
            container_home.join(fs_tools::SHARED_MEMORY_ROOT),
        ),
    ]
}

/// Creates the signpost directories and (re)writes their READMEs. The write is
/// unconditional so an edited text reaches existing installations at the next
/// container `ensure`, with no migration step — it is a few hundred bytes.
fn ensure_signposts(wd: &Path) -> Result<()> {
    for ((host, _), body) in signpost_mounts(wd, Path::new(CONTAINER_HOME))
        .iter()
        .zip([USER_MEMORY_SIGNPOST, SHARED_MEMORY_SIGNPOST])
    {
        std::fs::create_dir_all(host)
            .with_context(|| format!("failed to create signpost dir {}", host.display()))?;
        std::fs::write(host.join(SIGNPOST_README), body)
            .with_context(|| format!("failed to write signpost in {}", host.display()))?;
    }
    Ok(())
}

/// Owns the container lifecycle: the docker availability check, the runtime image,
/// and per-user create/start/stop/remove. Cheap to clone (holds an `Arc` pool).
#[derive(Clone)]
pub struct ContainerManager {
    system: Arc<SqlitePool>,
}

impl ContainerManager {
    pub fn new(system: Arc<SqlitePool>) -> Self {
        Self { system }
    }

    /// Fails if the Docker daemon is unreachable. Called at boot before anything
    /// else; a failure here stops the process (docker is REQUIRED).
    pub async fn check_docker(&self) -> Result<()> {
        match docker(&["version", "--format", "{{.Server.Version}}"]).await {
            Ok(v) => {
                crate::boot::section(format!("Docker ready (server {})", v.trim()));
                Ok(())
            }
            Err(e) => bail!(
                "Docker is REQUIRED but not available: {e}. \
                 Install Docker and ensure the daemon is running, then restart."
            ),
        }
    }

    /// Boot reconciliation: build the image if missing, then ensure every active
    /// user has a running container. Idempotent.
    pub async fn reconcile_all(&self) -> Result<()> {
        self.ensure_image().await?;
        let users = db::users::list(&self.system).await?;
        let mut started = 0usize;
        for user in &users {
            if !user.active {
                continue;
            }
            if let Err(e) = self.ensure(&user.id).await {
                tracing::error!(user = %user.id, error = %e, "failed to ensure user container");
            } else {
                started += 1;
            }
        }
        crate::boot::section(format!("User containers ready ({started})"));
        Ok(())
    }

    /// Builds the runtime image if the tag is absent. Writes the embedded
    /// Dockerfile to a temp dir and builds from there, so nothing is shipped
    /// beside the binary.
    pub async fn ensure_image(&self) -> Result<()> {
        if docker_ok(&["image", "inspect", IMAGE_TAG]).await {
            return Ok(());
        }
        crate::boot::section(format!("Building container image {IMAGE_TAG} (first run)…"));

        let dir = std::env::temp_dir().join(format!("skald-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).context("failed to create image build dir")?;
        std::fs::write(dir.join("Dockerfile"), DOCKERFILE).context("failed to write Dockerfile")?;

        let dir_str = dir.to_string_lossy().to_string();
        let out = docker(&["build", "-t", IMAGE_TAG, &dir_str]).await;
        let _ = std::fs::remove_dir_all(&dir);
        out.context("docker build failed")?;
        Ok(())
    }

    /// Ensures the user's container exists, runs as the host uid:gid, and is started.
    /// Creates the host directories, the container (if missing) with the right bind
    /// mounts + `--user`, and starts it (if stopped). Self-healing: a container whose
    /// `--user` no longer matches the host uid:gid (e.g. an old root container from a
    /// previous binary), that predates `--init`, or that runs a superseded
    /// [`IMAGE_TAG`], is torn down and recreated. Idempotent — a no-op when a matching
    /// container is already running.
    pub async fn ensure(&self, user_id: &str) -> Result<()> {
        let fs = build_user_fs(&self.system, user_id).await?;
        let wd = std::env::current_dir().context("failed to read working directory")?;

        // Host directories must exist before the mount, or Docker creates them
        // root-owned with surprising modes. Created by the host process, so they are
        // owned by the host uid:gid the container runs as — the mounts are writable.
        for (host, _container, _w) in fs.mounts() {
            std::fs::create_dir_all(&host)
                .with_context(|| format!("failed to create host dir {}", host.display()))?;
        }
        ensure_signposts(&wd)?;

        let name = &fs.container_name;
        let want_user = host_uid_gid().map(|(uid, gid)| format!("{uid}:{gid}"));

        match container_state(name).await {
            // Reuse only if it runs as the expected user AND has tini as PID 1;
            // otherwise recreate below.
            ContainerState::Running if reusable(name, &want_user).await => return Ok(()),
            ContainerState::Stopped if reusable(name, &want_user).await => {
                docker(&["start", name]).await.context("docker start failed")?;
                return Ok(());
            }
            ContainerState::Absent => {}
            // Present but stale — a mismatched `--user` (e.g. an old root container),
            // missing `--init` (an old container whose PID 1 is `sleep infinity`, which
            // ignores SIGTERM and hangs `docker stop` for the full grace, see
            // `SHUTDOWN_STOP_GRACE`), or an outdated image: tear it down. The container
            // holds no durable state — everything is in the bind mounts — so a recreate
            // is safe.
            _ => {
                let _ = docker(&["rm", "-f", name]).await;
            }
        }

        let mut args: Vec<String> = vec![
            "create".into(),
            // `--init` runs tini as pid 1 so orphaned/killed processes are reaped —
            // otherwise `execute_cmd`'s /stop reaper (and any command that leaves
            // orphans) would accumulate zombies under the idle `sleep infinity`.
            "--init".into(),
            "--name".into(),
            name.clone(),
            "--workdir".into(),
            fs.container_home.to_string_lossy().into_owned(),
        ];
        // Run as the host uid:gid for bind-mount ownership coherence (§6). HOME is set
        // explicitly because the passwd entry that resolves this uid is injected only
        // *after* create (see below), so Docker would otherwise default HOME to "/".
        if let Some(user) = &want_user {
            args.push("--user".into());
            args.push(user.clone());
            args.push("-e".into());
            args.push(format!("HOME={}", fs.container_home.to_string_lossy()));
        }
        for (host, container, writable) in fs.mounts() {
            let mut spec = format!("{}:{}", host.display(), container.display());
            if !writable {
                spec.push_str(":ro");
            }
            args.push("-v".into());
            args.push(spec);
        }
        // The virtual memory roots, read-only, nested inside the home mount (Docker
        // orders mounts by destination depth, as it already does for `shared/`).
        for (host, container) in signpost_mounts(&wd, &fs.container_home) {
            args.push("-v".into());
            args.push(format!("{}:{}:ro", host.display(), container.display()));
        }
        args.push(IMAGE_TAG.into());
        // Long-lived idle process; nothing runs until `docker exec` drives it.
        args.extend(["sleep".into(), "infinity".into()]);

        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        docker(&argv).await.context("docker create failed")?;
        docker(&["start", name]).await.context("docker start failed")?;

        // Give the non-root container user a passwd/group entry so `sudo` (NOPASSWD,
        // baked into the image) can resolve it. Persists in the container's writable
        // layer for its lifetime; re-done on recreate. Best-effort.
        if let Some((uid, gid)) = host_uid_gid() {
            ensure_container_user(name, uid, gid).await;
        }

        tracing::info!(user = %user_id, container = %name, "user container created and started");
        Ok(())
    }

    /// Stops every user's container (best-effort) at shutdown.
    ///
    /// Stops run **concurrently** and with a short [`SHUTDOWN_STOP_GRACE`], so total
    /// shutdown time is bounded by one grace window regardless of how many users there
    /// are — not `N × 10 s` as the old sequential, default-grace loop was (a pre-`--init`
    /// container ignores SIGTERM and burns the full grace before SIGKILL).
    pub async fn stop_all(&self) -> Result<()> {
        let users = db::users::list(&self.system).await?;
        let secs = SHUTDOWN_STOP_GRACE.as_secs().to_string();
        let mut set = tokio::task::JoinSet::new();
        for user in &users {
            let name = container_name(&user.id);
            let secs = secs.clone();
            set.spawn(async move {
                if let Err(e) = docker(&["stop", "-t", &secs, &name]).await {
                    tracing::debug!(container = %name, error = %e, "container stop (ignored)");
                }
            });
        }
        while set.join_next().await.is_some() {}
        Ok(())
    }

    /// Removes a user's container (force), e.g. on user deletion. Best-effort:
    /// a missing container is fine.
    pub async fn remove(&self, user_id: &str) -> Result<()> {
        let name = container_name(user_id);
        let _ = docker(&["rm", "-f", &name]).await;
        Ok(())
    }

    /// Gracefully shuts down a user's container: `docker stop` sends SIGTERM to the
    /// in-container processes and waits up to `STOP_GRACE` before SIGKILL, so an
    /// in-flight `execute_cmd` shell (and any per-user MCP `docker exec` child) gets
    /// a window to exit cleanly instead of vanishing mid-write. Best-effort: a
    /// missing or already-stopped container is fine.
    pub async fn stop(&self, user_id: &str) -> Result<()> {
        let name = container_name(user_id);
        let secs = STOP_GRACE.as_secs().to_string();
        if let Err(e) = docker(&["stop", "-t", &secs, &name]).await {
            tracing::debug!(container = %name, error = %e, "container stop (ignored)");
        }
        Ok(())
    }

    /// Cleanly recreates a user's container so it picks up a changed mount topology
    /// — e.g. a shared-folder membership change (§6), whose mounts are fixed at
    /// `docker create` time and cannot be altered on a live container. Graceful
    /// [`stop`](Self::stop) → remove → [`ensure`](Self::ensure) (which rebuilds the
    /// mount set from the current memberships and recreates the host dirs). The
    /// container holds no durable state — everything lives in the bind mounts — so a
    /// recreate is safe by construction. A no-op-safe `rm` (the container is already
    /// stopped) precedes `ensure`, which then finds it absent and creates it fresh.
    ///
    /// Caveat (caller's concern, not this method's): the per-user MCP runtime and a
    /// logged-in user's `UserFs` snapshot are both bound to the old container/
    /// membership and are NOT refreshed here — see the shared-folders remount wiring.
    pub async fn recreate(&self, user_id: &str) -> Result<()> {
        self.stop(user_id).await?;
        let _ = docker(&["rm", &container_name(user_id)]).await;
        self.ensure(user_id).await
    }
}

// ── docker CLI helpers ────────────────────────────────────────────────────────

#[derive(PartialEq)]
enum ContainerState {
    Running,
    Stopped,
    Absent,
}

/// Reads a container's running state via `docker inspect`.
async fn container_state(name: &str) -> ContainerState {
    match docker(&["inspect", "-f", "{{.State.Running}}", name]).await {
        Ok(out) if out.trim() == "true" => ContainerState::Running,
        Ok(_) => ContainerState::Stopped,
        Err(_) => ContainerState::Absent,
    }
}

/// Reads a container's configured `--user` (`docker inspect .Config.User`). Empty for a
/// container created without `--user` (i.e. root).
async fn container_user(name: &str) -> String {
    docker(&["inspect", "-f", "{{.Config.User}}", name])
        .await
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Whether a container's `--user` matches what we want. `want == None` (non-unix, no
/// `--user` requested) matches anything so we never churn a container needlessly.
async fn user_matches(name: &str, want: &Option<String>) -> bool {
    match want {
        None => true,
        Some(w) => &container_user(name).await == w,
    }
}

/// Whether a container was created with `--init` (tini as PID 1). `.HostConfig.Init`
/// is `true` only then; an old container (pre-`--init`) reports `<nil>`/`false`, so its
/// PID 1 is `sleep infinity`, which ignores SIGTERM and makes `docker stop` hang for
/// the full grace before SIGKILL. A `false` here triggers a recreate in [`ensure`].
async fn init_matches(name: &str) -> bool {
    docker(&["inspect", "-f", "{{.HostConfig.Init}}", name])
        .await
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// Whether a container runs the current [`IMAGE_TAG`]. A container pins the image it
/// was created from, so bumping the tag rebuilds the image but leaves every existing
/// container on the old one — the new tools would reach new users only. Comparing the
/// tag here turns the bump into a recreate, which is safe for the same reason the
/// `--user`/`--init` self-heal is: the container holds no durable state, everything
/// lives in the bind mounts. Unreadable inspect ⇒ `true`, so a docker hiccup never
/// churns a working container.
async fn image_matches(name: &str) -> bool {
    docker(&["inspect", "-f", "{{.Config.Image}}", name])
        .await
        .map(|s| s.trim() == IMAGE_TAG)
        .unwrap_or(true)
}

/// Whether a container carries the memory signpost mounts (see [`signpost_mounts`]).
/// Mounts are fixed at `docker create` time, so a container predating them keeps the
/// old, confusing view — bash silently writing into a `user-memory/` directory nobody
/// reads — until it is recreated. This is the fourth self-heal axis, and it is worth
/// its own check rather than an [`IMAGE_TAG`] bump: the image itself is unchanged, and
/// a bump would make every installation rebuild it to fix a mount. Unreadable inspect
/// ⇒ `true`, so a docker hiccup never churns a working container.
async fn signposts_mounted(name: &str) -> bool {
    let Ok(out) = docker(&["inspect", "-f", "{{range .Mounts}}{{println .Destination}}{{end}}", name]).await
    else {
        return true;
    };
    let dests: Vec<&str> = out.lines().map(str::trim).collect();
    signpost_mounts(Path::new(""), Path::new(CONTAINER_HOME))
        .iter()
        .all(|(_, container)| dests.iter().any(|d| Path::new(d) == container))
}

/// Whether an existing container can be reused as-is: right `--user` (§6 UID coherence),
/// `--init` (fast, clean `docker stop`), the current image **and** the memory signpost
/// mounts. A mismatch on any of the four recreates it.
async fn reusable(name: &str, want_user: &Option<String>) -> bool {
    user_matches(name, want_user).await
        && init_matches(name).await
        && image_matches(name).await
        && signposts_mounted(name).await
}

/// Gives the container's runtime `uid`/`gid` a passwd + shadow (+ group) entry, so
/// tools that resolve the invoking user work despite the arbitrary numeric uid — and
/// so `sudo` succeeds (without a shadow entry PAM's account phase fails with "account
/// validation failure" even under NOPASSWD). The shadow password is `*` (login
/// disabled, account valid); the group is added only when its gid is otherwise unused.
/// Runs as root inside the container (`-u 0`, which overrides the container's `--user`),
/// idempotent (keyed on the passwd entry), best-effort.
async fn ensure_container_user(name: &str, uid: u32, gid: u32) {
    let script = format!(
        "if ! getent passwd {uid} >/dev/null 2>&1; then \
           getent group {gid} >/dev/null 2>&1 || echo 'skald:x:{gid}:' >> /etc/group; \
           echo 'skald:x:{uid}:{gid}:skald:/root:/bin/sh' >> /etc/passwd; \
           echo 'skald:*:19000:0:99999:7:::' >> /etc/shadow; \
         fi"
    );
    if let Err(e) = docker(&["exec", "-u", "0", name, "sh", "-c", &script]).await {
        tracing::warn!(container = %name, error = %e, "failed to inject container passwd entry (sudo may not resolve the user)");
    }
}

/// Runs `docker <args>`, returning trimmed stdout on success or an error carrying
/// stderr. `stdin` is closed so a build never blocks waiting for input.
async fn docker(args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to spawn `docker` (is the Docker CLI installed?)")?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&output.stderr);
        bail!("docker {:?} failed: {}", args, err.trim());
    }
}

/// True when `docker <args>` exits zero. For probes (`image inspect`,
/// `container inspect`) where a non-zero exit just means "absent".
async fn docker_ok(args: &[&str]) -> bool {
    tokio::process::Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}
