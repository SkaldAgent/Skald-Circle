//! Per-user filesystem mapping (blueprint §6): the bridge between the path an
//! agent sees and the physical host / container path behind it.
//!
//! An agent sees one namespace:
//!
//! | Agent path        | Backing                                            |
//! |-------------------|----------------------------------------------------|
//! | `user-memory/…`   | SQLite (the user's pool) — routed *before* this    |
//! | `shared-memory/…` | SQLite (`system.db`) — routed *before* this        |
//! | `shared/{X}/…`    | host `{WD}/shared/{X}`, mount `{home}/shared/{X}`  |
//! | `~/…`, relative   | host `{WD}/homes/{userid}`, mount `{container_home}`|
//!
//! `UserFs` is a **pure value type** with no filesystem access: it carries the
//! resolved paths and does the lexical agent→host / agent→container mapping.
//! Containment (canonicalize + prefix-check against the mount root, which follows
//! symlinks) lives in `skald-core`, where the fs helpers already are — this crate
//! stays dependency-light. The two virtual memory roots are classified by the
//! fs-tools *before* reaching here; `UserFs` only ever sees physical paths.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

/// One shared folder mounted into a user's container.
#[derive(Debug, Clone)]
pub struct SharedMount {
    /// The folder name (`{WD}/shared/{name}`); the first path component under `shared/`.
    pub name:      String,
    /// Absolute host directory that backs it.
    pub host:      PathBuf,
    /// Where it is mounted inside the container.
    pub container: PathBuf,
    /// Whether this member may write to it.
    pub can_write: bool,
}

/// The filesystem view of one user: their private home plus the shared folders
/// they belong to, and the container those are mounted into.
#[derive(Debug, Clone)]
pub struct UserFs {
    pub user_id:        String,
    /// Absolute host path of the user's private home (`{WD}/homes/{userid}`).
    pub home_host:      PathBuf,
    /// The Docker container name for this user (`skald-{userid}`).
    pub container_name: String,
    /// The home mount point inside the container (e.g. `/root`).
    pub container_home: PathBuf,
    /// Shared folders this user can reach, in name order.
    pub shared:         Vec<SharedMount>,
}

impl UserFs {
    pub fn new(
        user_id:        impl Into<String>,
        home_host:      PathBuf,
        container_name: impl Into<String>,
        container_home: PathBuf,
        shared:         Vec<SharedMount>,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            home_host,
            container_name: container_name.into(),
            container_home,
            shared,
        }
    }

    /// Look up a shared mount by its folder name.
    pub fn shared_mount(&self, name: &str) -> Option<&SharedMount> {
        self.shared.iter().find(|m| m.name == name)
    }

    /// The bind mounts for `docker create`: `(host, container, writable)`, home first.
    pub fn mounts(&self) -> Vec<(PathBuf, PathBuf, bool)> {
        let mut out = vec![(self.home_host.clone(), self.container_home.clone(), true)];
        for m in &self.shared {
            out.push((m.host.clone(), m.container.clone(), m.can_write));
        }
        out
    }

    /// The host base a physical agent path resolves against, and the tail relative
    /// to it — **without** touching the filesystem. `shared/{X}/…` resolves against
    /// the shared mount's host dir (only if the user is a member); everything else
    /// resolves against the private home. Returns `None` when the path names a
    /// `shared/` folder the user does not belong to. The caller (skald-core) then
    /// joins + canonicalizes + prefix-checks against the returned base.
    ///
    /// Memory paths (`user-memory/…`, `shared-memory/…`) must be classified and
    /// routed to SQLite *before* calling this — they are not physical paths.
    pub fn host_base_and_tail<'a>(&self, agent_path: &'a str) -> Option<(PathBuf, String)> {
        let stripped = strip_home_prefix(agent_path);
        let mut parts = stripped.splitn(2, ['/', '\\']);
        match parts.next() {
            Some("shared") => {
                let rest = parts.next().unwrap_or("");
                let mut seg = rest.splitn(2, ['/', '\\']);
                let name = seg.next().unwrap_or("");
                let tail = seg.next().unwrap_or("");
                let mount = self.shared_mount(name)?;
                Some((mount.host.clone(), tail.to_string()))
            }
            _ => Some((self.home_host.clone(), stripped.to_string())),
        }
    }

    /// Map an agent path to its **container** path (pure, lexical): `~`/relative →
    /// under `container_home`; `shared/{X}` → under `container_home/shared/{X}`; an
    /// already-absolute path is taken as a container path as-is. Used to set the
    /// working directory of an `execute_cmd` inside the container.
    pub fn to_container(&self, agent_path: &str) -> PathBuf {
        let p = Path::new(agent_path);
        if p.is_absolute() {
            return normalize(p);
        }
        let stripped = strip_home_prefix(agent_path);
        normalize(&self.container_home.join(stripped))
    }
}

/// Strips a leading `~/`, bare `~`, or `./` so what remains is relative to the home.
fn strip_home_prefix(path: &str) -> &str {
    if let Some(rest) = path.strip_prefix("~/") {
        rest
    } else if path == "~" {
        ""
    } else {
        path.trim_start_matches("./")
    }
}

/// A hot-swappable handle to a [`UserFs`] snapshot, shared by every holder that
/// must observe a membership change without being rebuilt (blueprint §6 remount).
///
/// Cloning shares the *same* cell. `store` replaces the snapshot for all clones at
/// once; each `load` returns the current `Arc<UserFs>`. A live chat session's
/// handler holds a clone, so a shared-folder change reaches it on its next tool
/// call — no handler eviction, and no cross-session race (the swap is a single
/// pointer store behind the lock, and each `ToolContext` takes a consistent
/// snapshot for the duration of its call).
#[derive(Clone)]
pub struct SharedFs(Arc<RwLock<Arc<UserFs>>>);

impl SharedFs {
    pub fn new(fs: UserFs) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(fs))))
    }

    /// The current snapshot. Cheap — clones an `Arc`.
    pub fn load(&self) -> Arc<UserFs> {
        Arc::clone(&self.0.read().expect("SharedFs lock poisoned"))
    }

    /// Replace the snapshot seen by every holder of this cell.
    pub fn store(&self, fs: UserFs) {
        *self.0.write().expect("SharedFs lock poisoned") = Arc::new(fs);
    }
}

impl std::fmt::Debug for SharedFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SharedFs").field(&*self.load()).finish()
    }
}

/// Pure lexical normalization (resolve `.`/`..`), no filesystem access.
fn normalize(p: &Path) -> PathBuf {
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
