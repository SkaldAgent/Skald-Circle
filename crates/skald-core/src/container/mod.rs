//! Per-user Docker containers (blueprint §6): the execution sandbox.
//!
//! Each user gets one **permanent** container (`skald-{userid}`), built from our
//! own image (`skald-runtime`, python + node). The container is created when the
//! user is created and started at application boot; `execute_cmd` and — later —
//! the user's stateful MCP servers run inside it, against the user's bind-mounted
//! home (`{WD}/homes/{userid}` → `/root`) plus the shared folders they belong to.
//!
//! Docker is a **hard requirement**: [`ContainerManager::check_docker`] fails
//! construction if the daemon is unreachable, and the shell exits at boot.
//!
//! We shell out to the `docker` CLI rather than link a Docker client crate: fewer
//! dependencies, and the same process-spawning shape `execute_cmd` already uses.
//! The container holds no durable state — everything lives in the bind mounts — so
//! a container can be recreated from the image at any time; boot reconciliation
//! relies on that.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sqlx::SqlitePool;

use core_api::user_fs::{SharedMount, UserFs};

use crate::db;

/// Our runtime image tag. Built once from the embedded [`Dockerfile`].
const IMAGE_TAG: &str = "skald-runtime";

/// The embedded Dockerfile — the source of truth, so the image can be built with
/// no files shipped alongside the binary (binary-first).
const DOCKERFILE: &str = include_str!("Dockerfile");

/// Subdirectory of the working directory holding per-user homes.
pub const HOMES_DIR: &str = "homes";
/// Subdirectory of the working directory holding shared folders.
pub const SHARED_DIR: &str = "shared";
/// Home mount point inside the container.
pub const CONTAINER_HOME: &str = "/root";
/// Grace window `docker stop` gives in-container processes (SIGTERM → SIGKILL)
/// before force-killing — enough for a shell or MCP `docker exec` child to exit.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// The deterministic container name for a user — derivable without any manager,
/// so `UserFs` can carry it and `execute_cmd` can exec into it directly.
pub fn container_name(user_id: &str) -> String {
    format!("skald-{user_id}")
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

    Ok(UserFs::new(user_id, home_host, container_name(user_id), container_home, shared))
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

    /// Ensures the user's container exists and is running. Creates the host
    /// directories, the container (if missing) with the right bind mounts, and
    /// starts it (if stopped). Idempotent — a no-op when already running.
    pub async fn ensure(&self, user_id: &str) -> Result<()> {
        let fs = build_user_fs(&self.system, user_id).await?;

        // Host directories must exist before the mount, or Docker creates them
        // root-owned with surprising modes.
        for (host, _container, _w) in fs.mounts() {
            std::fs::create_dir_all(&host)
                .with_context(|| format!("failed to create host dir {}", host.display()))?;
        }

        let name = &fs.container_name;
        match container_state(name).await {
            ContainerState::Running => return Ok(()),
            ContainerState::Stopped => {
                docker(&["start", name]).await.context("docker start failed")?;
                return Ok(());
            }
            ContainerState::Absent => {}
        }

        let mut args: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            name.clone(),
            "--workdir".into(),
            fs.container_home.to_string_lossy().into_owned(),
        ];
        for (host, container, writable) in fs.mounts() {
            let mut spec = format!("{}:{}", host.display(), container.display());
            if !writable {
                spec.push_str(":ro");
            }
            args.push("-v".into());
            args.push(spec);
        }
        args.push(IMAGE_TAG.into());
        // Long-lived idle process; nothing runs until `docker exec` drives it.
        args.extend(["sleep".into(), "infinity".into()]);

        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        docker(&argv).await.context("docker create failed")?;
        docker(&["start", name]).await.context("docker start failed")?;
        tracing::info!(user = %user_id, container = %name, "user container created and started");
        Ok(())
    }

    /// Stops every user's container (best-effort) at shutdown.
    pub async fn stop_all(&self) -> Result<()> {
        let users = db::users::list(&self.system).await?;
        for user in &users {
            let name = container_name(&user.id);
            if let Err(e) = docker(&["stop", &name]).await {
                tracing::debug!(container = %name, error = %e, "container stop (ignored)");
            }
        }
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
