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
//! | `projects/{O}/{S}`| host `{WD}/projects/{owner_userid}/{S}`, mount `{home}/projects/{O}/{S}` (O = owner username) |
//! | `~/docs/…`, `docs/…` | host `{WD}/docs` (read-only, same for every user), mount `{container_home}/docs` |
//! | `skills/…`        | the read-only skills tree — see [`SkillMounts`]     |
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

/// The subdirectory of a user's home where chat uploads are saved
/// (`{home}/uploads/{session_id}/…`, reachable by the agent as `uploads/…`).
/// Shared by the upload handler (write path) and the media inliner (containment
/// root) so the two anchors can never drift.
pub const UPLOADS_SUBDIR: &str = "uploads";

/// The single top-level agent path under which every skill lives. Reserved: a
/// path starting with this segment never falls back to the home, whatever
/// follows it (see [`UserFs::host_base_and_tail`]).
pub const SKILLS_ROOT: &str = "skills";

/// The scope segment of the group-wide skills, `skills/shared/<id>`. The other
/// scope segment is the owner's own username, which is data, not a constant.
pub const SKILLS_SHARED_SCOPE: &str = "shared";

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

/// One project folder mounted into a user's container. Unlike a shared folder its
/// agent path has **two** segments — `projects/{owner_username}/{slug}` — because a
/// project is namespaced by its owner (two members can each own a `budget`). The host
/// path keys on the owner's stable **userid**, the agent/container path on the
/// (mutable) **username**.
#[derive(Debug, Clone)]
pub struct ProjectMount {
    /// The owner's username — the first agent-visible segment under `projects/`.
    pub owner_username: String,
    /// The project slug — the second agent-visible segment.
    pub slug:           String,
    /// Absolute host directory that backs it (`{WD}/projects/{owner_userid}/{slug}`).
    pub host:           PathBuf,
    /// Where it is mounted inside the container (`{home}/projects/{owner_username}/{slug}`).
    pub container:      PathBuf,
    /// Whether this member may write to it.
    pub can_write:      bool,
}

/// The skills tree of one user: a single agent root, `skills/`, with two scope
/// subtrees below it — `skills/shared/<id>` (the group's, curated) and
/// `skills/<username>/<id>` (this member's own). The agent path carries the
/// **username** while the host path keys on the stable **userid**, exactly as
/// `projects/{owner_username}/{slug}` already does.
///
/// **Everything here is read-only for the agent, in both directions**: `:ro` bind
/// mounts in the container and [`UserFs::can_write_to`] false host-side. These are
/// not working folders — they hold installed artefacts, and the only door in is the
/// registration tool.
///
/// The three host paths are one field rather than three `Option`s because they
/// cannot exist apart. Docker refuses to create a mountpoint inside a `:ro` bind
/// mount (`mkdirat … read-only file system`, at container create), so the two scope
/// mounts nest inside the root mount only if `shared/` and `<username>/` already
/// exist **in the root mount's own source directory**. That forces the root to be
/// per-user (the username segment differs) and forces it to be materialized
/// together with the scopes it carries.
#[derive(Debug, Clone)]
pub struct SkillMounts {
    /// Host dir mounted at `{container_home}/skills` (`{WD}/.skills-root/{userid}`).
    /// Holds the signpost README plus the two empty scope mountpoints, and nothing
    /// else: its job is to make the space *between* the scopes read-only too, so an
    /// invented scope segment fails loudly instead of landing somewhere unread.
    pub root_host:    PathBuf,
    /// Host dir behind `skills/shared/…` (`{WD}/skills`), the same for every user.
    pub shared_host:  PathBuf,
    /// Host dir behind `skills/{own_username}/…` (`{WD}/skills-users/{userid}`).
    pub own_host:     PathBuf,
    /// The owner's username — the agent-visible segment of their own scope.
    pub own_username: String,
}

impl SkillMounts {
    /// The container path of the root mount, given the home mount point.
    pub fn container_root(&self, container_home: &Path) -> PathBuf {
        container_home.join(SKILLS_ROOT)
    }

    /// The container paths of the two scope mounts, which nest inside the root.
    pub fn container_scopes(&self, container_home: &Path) -> [PathBuf; 2] {
        let root = self.container_root(container_home);
        [root.join(SKILLS_SHARED_SCOPE), root.join(&self.own_username)]
    }
}

/// Why an agent path does not resolve to a host location.
///
/// This exists because the wrong doors under `skills/` each need to say something
/// different, and a bare `None` could only ever produce one sentence. Saying the
/// right one matters more here than elsewhere: the whole root is read-only, so a
/// model that guesses a scope gets a refusal, and a refusal that does not name the
/// right path is answered with `sudo`.
#[derive(Debug, Clone, PartialEq)]
pub enum RouteError {
    /// Not reachable, and this is the message to show the model.
    Denied(String),
    /// `skills/<id>/<tail>` where `<id>` is neither `shared` nor the owner's
    /// username — so it may be the tolerant bare-id alias, the shortest spelling
    /// and therefore the one a model produces on its own.
    ///
    /// Resolving it means knowing which of the two trees actually holds `<id>`,
    /// i.e. touching the filesystem, which this pure value type must not do. The
    /// caller (skald-core's `resolve_host_path`) probes and either resolves it or
    /// reports — including the ambiguous case, which fails loudly listing both
    /// full paths rather than letting either tree win in silence.
    SkillAlias { id: String, tail: String },
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::Denied(msg) => f.write_str(msg),
            RouteError::SkillAlias { id, .. } => {
                write!(f, "no skill named `{id}`")
            }
        }
    }
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
    /// Projects this user can reach (owned + shared-with-them), by owner then slug.
    pub projects:       Vec<ProjectMount>,
    /// Host directory backing the read-only docs mount (`{WD}/docs`), the same for
    /// every user. `None` when unset (inert placeholders, unit tests that don't
    /// touch it) — `docs/…` then resolves like any other unmounted path.
    pub docs_host:      Option<PathBuf>,
    /// The read-only skills tree (see [`SkillMounts`]). `None` for the inert
    /// placeholders and unit tests that don't touch it — `skills/…` is then
    /// refused outright, never routed to the home.
    pub skills:         Option<SkillMounts>,
}

impl UserFs {
    pub fn new(
        user_id:        impl Into<String>,
        home_host:      PathBuf,
        container_name: impl Into<String>,
        container_home: PathBuf,
        shared:         Vec<SharedMount>,
        projects:       Vec<ProjectMount>,
        docs_host:      Option<PathBuf>,
    ) -> Self {
        Self {
            user_id: user_id.into(),
            home_host,
            container_name: container_name.into(),
            container_home,
            shared,
            projects,
            docs_host,
            skills: None,
        }
    }

    /// Attach the skills tree. A builder step rather than an eighth constructor
    /// argument: only the real per-user build has one, and every inert or test
    /// `UserFs` is honestly skill-less.
    pub fn with_skills(mut self, skills: SkillMounts) -> Self {
        self.skills = Some(skills);
        self
    }

    /// Look up a shared mount by its folder name.
    pub fn shared_mount(&self, name: &str) -> Option<&SharedMount> {
        self.shared.iter().find(|m| m.name == name)
    }

    /// Look up a project mount by its owner username + slug (the two agent segments).
    pub fn project_mount(&self, owner_username: &str, slug: &str) -> Option<&ProjectMount> {
        self.projects
            .iter()
            .find(|m| m.owner_username == owner_username && m.slug == slug)
    }

    /// Whether the user may **write** at this agent path: their home → always;
    /// a shared-folder or project mount → the membership's `can_write` flag;
    /// `docs/…` and **anything under `skills/`** → never (read-only). A
    /// `shared/`/`projects/` mount the user is not a member of → false
    /// (fail-closed, same as the read side). Purely lexical: memory paths never
    /// reach here (classified earlier).
    ///
    /// The `skills` arm covers the **whole root**, not the two known scopes, and
    /// that width is the point: the fallthrough below answers `true`, so a scope
    /// segment the model invented (`skills/pippo/SKILL.md`) would otherwise be
    /// writable — and would land in a physical directory under the home that no
    /// indexer ever reads. That is the memory-signpost failure exactly, and it is
    /// closed here and, for the shell's half, by the root `:ro` mount.
    pub fn can_write_to(&self, agent_path: &str) -> bool {
        let stripped = strip_home_prefix(agent_path);
        let mut parts = stripped.splitn(2, ['/', '\\']);
        match parts.next() {
            Some("shared") => {
                let rest = parts.next().unwrap_or("");
                let name = rest.splitn(2, ['/', '\\']).next().unwrap_or("");
                self.shared_mount(name).map(|m| m.can_write).unwrap_or(false)
            }
            Some("projects") => {
                let rest = parts.next().unwrap_or("");
                let mut seg = rest.splitn(3, ['/', '\\']);
                let owner = seg.next().unwrap_or("");
                let slug  = seg.next().unwrap_or("");
                self.project_mount(owner, slug).map(|m| m.can_write).unwrap_or(false)
            }
            Some("docs") => false,
            // The entire skills root, `self.skills` set or not: the name is
            // reserved, so a context without the mounts must refuse rather than
            // silently offer a home directory of the same name.
            Some(SKILLS_ROOT) => false,
            _ => true,
        }
    }

    /// The bind mounts for `docker create`: `(host, container, writable)`, home first.
    ///
    /// Emitted in **destination-depth order**, which the skills tree is the first to
    /// actually need: its two scope mounts nest inside its root mount, and the root
    /// must be in place before them.
    pub fn mounts(&self) -> Vec<(PathBuf, PathBuf, bool)> {
        let mut out = vec![(self.home_host.clone(), self.container_home.clone(), true)];
        for m in &self.shared {
            out.push((m.host.clone(), m.container.clone(), m.can_write));
        }
        for m in &self.projects {
            out.push((m.host.clone(), m.container.clone(), m.can_write));
        }
        if let Some(docs) = &self.docs_host {
            out.push((docs.clone(), self.container_home.join("docs"), false));
        }
        if let Some(sk) = &self.skills {
            let [shared, own] = sk.container_scopes(&self.container_home);
            out.push((sk.root_host.clone(),   sk.container_root(&self.container_home), false));
            out.push((sk.shared_host.clone(), shared,                                  false));
            out.push((sk.own_host.clone(),    own,                                     false));
        }
        out
    }

    /// The host base a physical agent path resolves against, and the tail relative
    /// to it — **without** touching the filesystem. `shared/{X}/…` resolves against
    /// the shared mount's host dir (only if the user is a member); `skills/…`
    /// against the skills tree; everything else against the private home. The
    /// caller (skald-core) then joins + canonicalizes + prefix-checks against the
    /// returned base.
    ///
    /// Memory paths (`user-memory/…`, `shared-memory/…`) must be classified and
    /// routed to SQLite *before* calling this — they are not physical paths.
    pub fn host_base_and_tail(&self, agent_path: &str) -> Result<(PathBuf, String), RouteError> {
        let stripped = strip_home_prefix(agent_path);
        let mut parts = stripped.splitn(2, ['/', '\\']);
        match parts.next() {
            Some("shared") => {
                let rest = parts.next().unwrap_or("");
                let mut seg = rest.splitn(2, ['/', '\\']);
                let name = seg.next().unwrap_or("");
                let tail = seg.next().unwrap_or("");
                let mount = self.shared_mount(name).ok_or_else(|| {
                    RouteError::Denied(format!(
                        "no such shared folder, or you are not a member: {agent_path}"
                    ))
                })?;
                Ok((mount.host.clone(), tail.to_string()))
            }
            Some("projects") => {
                // Two segments: `projects/{owner_username}/{slug}/{tail…}`.
                let rest = parts.next().unwrap_or("");
                let mut seg = rest.splitn(3, ['/', '\\']);
                let owner = seg.next().unwrap_or("");
                let slug  = seg.next().unwrap_or("");
                let tail  = seg.next().unwrap_or("");
                let mount = self.project_mount(owner, slug).ok_or_else(|| {
                    RouteError::Denied(format!(
                        "no such project, or you are not a member: {agent_path}"
                    ))
                })?;
                Ok((mount.host.clone(), tail.to_string()))
            }
            Some("docs") => {
                let host = self.docs_host.clone().ok_or_else(|| {
                    RouteError::Denied(format!("docs are not available here: {agent_path}"))
                })?;
                let tail = parts.next().unwrap_or("");
                Ok((host, tail.to_string()))
            }
            Some(SKILLS_ROOT) => self.route_skills(agent_path, parts.next().unwrap_or("")),
            _ => Ok((self.home_host.clone(), stripped.to_string())),
        }
    }

    /// Routes everything under the reserved `skills/` root. Split out because it is
    /// the one branch that must never fall through to the home: `skills/` names a
    /// tree the user cannot write to and only partly owns, so the answer to an
    /// unrecognised second segment is an error — never a home path that quietly
    /// accepts a write nobody will ever read back.
    fn route_skills(&self, agent_path: &str, rest: &str) -> Result<(PathBuf, String), RouteError> {
        let Some(sk) = &self.skills else {
            return Err(RouteError::Denied(format!(
                "skills are not available in this context: {agent_path}"
            )));
        };
        let mut seg = rest.splitn(2, ['/', '\\']);
        let scope = seg.next().unwrap_or("");
        let tail  = seg.next().unwrap_or("");
        if scope.is_empty() {
            // `skills` / `skills/` itself: the root mount, which holds the signpost.
            return Ok((sk.root_host.clone(), String::new()));
        }
        if scope == SKILLS_SHARED_SCOPE {
            return Ok((sk.shared_host.clone(), tail.to_string()));
        }
        if scope == sk.own_username {
            return Ok((sk.own_host.clone(), tail.to_string()));
        }
        Err(RouteError::SkillAlias { id: scope.to_string(), tail: tail.to_string() })
    }

    /// The two scope trees a bare `skills/<id>` alias may resolve in, as
    /// `(agent path of the candidate, host path to probe)`. Pure: the caller checks
    /// which of them exist. Ordered shared-then-own only so the ambiguity message
    /// reads the same every time — neither wins.
    pub fn skill_alias_candidates(&self, id: &str) -> Vec<(String, PathBuf)> {
        let Some(sk) = &self.skills else { return Vec::new() };
        vec![
            (
                format!("{SKILLS_ROOT}/{SKILLS_SHARED_SCOPE}/{id}"),
                sk.shared_host.join(id),
            ),
            (
                format!("{SKILLS_ROOT}/{}/{id}", sk.own_username),
                sk.own_host.join(id),
            ),
        ]
    }

    /// The message for a `skills/<seg>/…` that is neither a known scope nor an
    /// installed skill id.
    ///
    /// One sentence covers all three wrong doors — an invented scope, a typo'd id,
    /// and another member's tree — because `UserFs` knows only its owner's username
    /// and cannot tell a stranger's name from nonsense. Naming what *is* reachable,
    /// including the fact that other members' skills are not, answers the question
    /// behind each of them without pretending to know which one was asked.
    pub fn skill_route_hint(&self, id: &str) -> String {
        match &self.skills {
            Some(sk) => format!(
                "no skill named `{id}`. Skills live in `{SKILLS_ROOT}/{SKILLS_SHARED_SCOPE}/<id>/` \
                 (the group's) and `{SKILLS_ROOT}/{}/<id>/` (yours); other members' skills are \
                 not accessible, and `{SKILLS_ROOT}/` has no other subfolders.",
                sk.own_username
            ),
            None => format!("skills are not available in this context: {SKILLS_ROOT}/{id}"),
        }
    }

    /// Map an agent path to its **container** path (pure, lexical): `~`/relative →
    /// under `container_home`; `shared/{X}` and `projects/{O}/{S}` → under
    /// `container_home/…` (they mirror the container layout); an already-absolute path
    /// is taken as a container path as-is. Used to set the working directory of an
    /// `execute_cmd` inside the container.
    pub fn to_container(&self, agent_path: &str) -> PathBuf {
        let p = Path::new(agent_path);
        if p.is_absolute() {
            return normalize(p);
        }
        let stripped = strip_home_prefix(agent_path);
        normalize(&self.container_home.join(stripped))
    }

    /// Reverse of [`to_container`](Self::to_container) for an already-absolute path:
    /// map a **container-absolute** path (`/root/…`, `/root/shared/{X}/…`,
    /// `/root/projects/{O}/{S}/…`, `/root/skills/…`) back to the agent vocabulary.
    /// Shared, project and skill mounts nest *under* `container_home`, so they are
    /// matched **first** — otherwise `/root/shared/X` would strip against the home
    /// base and come back as `~/shared/X`, a spelling that routes correctly but is
    /// not the canonical one the viewer keys on. Within the skills tree the two
    /// scopes are matched before the root, which is their prefix.
    ///
    /// Returns `None` when `abs` lies outside every one of this user's container mounts
    /// (i.e. it points outside their view) — the caller rejects it fail-closed. Purely
    /// lexical: no membership check, no filesystem access.
    pub fn container_to_agent(&self, abs: &Path) -> Option<String> {
        let abs = normalize(abs);
        for m in &self.shared {
            if let Ok(tail) = abs.strip_prefix(&m.container) {
                return Some(agent_join(&format!("shared/{}", m.name), tail));
            }
        }
        for m in &self.projects {
            if let Ok(tail) = abs.strip_prefix(&m.container) {
                return Some(agent_join(&format!("projects/{}/{}", m.owner_username, m.slug), tail));
            }
        }
        if let Some(sk) = &self.skills {
            let [shared, own] = sk.container_scopes(&self.container_home);
            if let Ok(tail) = abs.strip_prefix(&shared) {
                return Some(agent_join(&format!("{SKILLS_ROOT}/{SKILLS_SHARED_SCOPE}"), tail));
            }
            if let Ok(tail) = abs.strip_prefix(&own) {
                return Some(agent_join(&format!("{SKILLS_ROOT}/{}", sk.own_username), tail));
            }
            if let Ok(tail) = abs.strip_prefix(sk.container_root(&self.container_home)) {
                return Some(agent_join(SKILLS_ROOT, tail));
            }
        }
        abs.strip_prefix(&self.container_home)
            .ok()
            .map(|tail| agent_join("~", tail))
    }

    /// Normalize any path arriving from the show-file / file-viewer surface into a
    /// **canonical agent path** the UI can display and echo back: a relative or `~/…`
    /// path is cleaned and rooted (`report.md` → `~/report.md`, `shared/X/y` and
    /// `projects/O/S/y` keep their root); a container-absolute path is reverse-mapped
    /// via [`container_to_agent`](Self::container_to_agent).
    ///
    /// Returns `None` only for an absolute path outside every container mount — the
    /// caller rejects it fail-closed. Purely lexical (`.`/`..` collapse, `..` clamps at
    /// the root); membership + on-disk containment are enforced later, in skald-core.
    pub fn to_agent_display(&self, input: &str) -> Option<String> {
        let p = Path::new(input);
        if p.is_absolute() {
            return self.container_to_agent(p);
        }
        let cleaned = normalize(Path::new(strip_home_prefix(input)));
        let cleaned = cleaned.to_string_lossy().replace('\\', "/");
        let root = cleaned.split('/').next().unwrap_or("");
        if root == "shared" || root == "projects" || root == SKILLS_ROOT {
            Some(cleaned)
        } else if cleaned.is_empty() {
            Some("~".to_string())
        } else {
            Some(format!("~/{cleaned}"))
        }
    }
}

/// Join an agent-path base (`~`, `shared/X`, `projects/O/S`) with a tail relative to
/// the mount, normalizing separators. An empty tail yields the bare base.
fn agent_join(base: &str, tail: &Path) -> String {
    let t = tail.to_string_lossy().replace('\\', "/");
    if t.is_empty() { base.to_string() } else { format!("{base}/{t}") }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fs_with_skills() -> UserFs {
        UserFs::new(
            "u1",
            PathBuf::from("/wd/homes/u1"),
            "skald-u1",
            PathBuf::from("/root"),
            vec![],
            vec![],
            None,
        )
        .with_skills(SkillMounts {
            root_host:    PathBuf::from("/wd/.skills-root/u1"),
            shared_host:  PathBuf::from("/wd/skills"),
            own_host:     PathBuf::from("/wd/skills-users/u1"),
            own_username: "daniele".into(),
        })
    }

    /// The two scopes route to their own host trees, whichever way the agent spells
    /// the home prefix.
    #[test]
    fn skill_scopes_route_to_their_trees() {
        let fs = fs_with_skills();
        for spelling in ["skills/shared/ics/SKILL.md", "~/skills/shared/ics/SKILL.md", "./skills/shared/ics/SKILL.md"] {
            assert_eq!(
                fs.host_base_and_tail(spelling).unwrap(),
                (PathBuf::from("/wd/skills"), "ics/SKILL.md".to_string()),
                "{spelling}"
            );
        }
        assert_eq!(
            fs.host_base_and_tail("skills/daniele/spesa/run.py").unwrap(),
            (PathBuf::from("/wd/skills-users/u1"), "spesa/run.py".to_string())
        );
        // The root itself is the signpost mount, not the home.
        assert_eq!(
            fs.host_base_and_tail("skills").unwrap(),
            (PathBuf::from("/wd/.skills-root/u1"), String::new())
        );
    }

    /// An invented scope segment must never fall back to the home — that fallback is
    /// what turns `skills/pippo/SKILL.md` into a real file under `homes/u1/` that no
    /// indexer ever reads. It comes back as an alias candidate for the caller to
    /// probe, and there is no third answer.
    #[test]
    fn an_unknown_scope_never_falls_back_to_the_home() {
        let fs = fs_with_skills();
        match fs.host_base_and_tail("skills/pippo/SKILL.md") {
            Err(RouteError::SkillAlias { id, tail }) => {
                assert_eq!(id, "pippo");
                assert_eq!(tail, "SKILL.md");
            }
            other => panic!("expected an alias probe, got {other:?}"),
        }
        // Another member's tree lands in the same branch, and the hint says so.
        match fs.host_base_and_tail("skills/serena/x/SKILL.md") {
            Err(RouteError::SkillAlias { id, .. }) => {
                let hint = fs.skill_route_hint(&id);
                assert!(hint.contains("other members' skills are not accessible"), "{hint}");
                assert!(hint.contains("skills/daniele/<id>/"), "{hint}");
            }
            other => panic!("expected an alias probe, got {other:?}"),
        }
        // Without a skills tree at all the root is still reserved, never the home.
        let bare = UserFs::new("u1", PathBuf::from("/wd/homes/u1"), "c", PathBuf::from("/root"), vec![], vec![], None);
        assert!(bare.host_base_and_tail("skills/shared/x").is_err());
    }

    /// The whole root is read-only, including the space between the two scopes and
    /// including a context that has no skills tree at all.
    #[test]
    fn nothing_under_the_skills_root_is_writable() {
        let fs = fs_with_skills();
        for p in [
            "skills",
            "skills/README.md",
            "skills/shared/ics/SKILL.md",
            "skills/daniele/spesa/SKILL.md",
            "skills/pippo/SKILL.md",
            "~/skills/pippo/SKILL.md",
        ] {
            assert!(!fs.can_write_to(p), "{p} should be read-only");
        }
        // The home around it is unaffected.
        assert!(fs.can_write_to("~/notes.md"));
        assert!(fs.can_write_to("skillset/notes.md"));

        let bare = UserFs::new("u1", PathBuf::from("/wd/homes/u1"), "c", PathBuf::from("/root"), vec![], vec![], None);
        assert!(!bare.can_write_to("skills/anything"));
    }

    /// The scope mounts nest inside the root mount, so they must be matched first —
    /// otherwise the root (their own prefix) claims them, and the home claims all
    /// three.
    #[test]
    fn container_paths_map_back_to_the_scope_that_owns_them() {
        let fs = fs_with_skills();
        assert_eq!(fs.container_to_agent(Path::new("/root/skills/shared/ics/SKILL.md")).unwrap(), "skills/shared/ics/SKILL.md");
        assert_eq!(fs.container_to_agent(Path::new("/root/skills/daniele/spesa")).unwrap(), "skills/daniele/spesa");
        assert_eq!(fs.container_to_agent(Path::new("/root/skills/README.md")).unwrap(), "skills/README.md");
        assert_eq!(fs.container_to_agent(Path::new("/root/skills")).unwrap(), "skills");
        assert_eq!(fs.container_to_agent(Path::new("/root/notes.md")).unwrap(), "~/notes.md");
        // And the display form keeps the skills root rather than re-rooting on `~`.
        assert_eq!(fs.to_agent_display("skills/shared/ics").unwrap(), "skills/shared/ics");
        assert_eq!(fs.to_agent_display("~/skills/shared/ics").unwrap(), "skills/shared/ics");
    }

    /// Docker cannot create a mountpoint inside a `:ro` mount, so the root has to be
    /// mounted before the two scopes that nest in it — and all three read-only.
    #[test]
    fn skill_mounts_are_read_only_and_root_first() {
        let fs = fs_with_skills();
        let mounts = fs.mounts();
        let skills: Vec<_> = mounts
            .iter()
            .filter(|(_, container, _)| container.starts_with("/root/skills"))
            .collect();
        assert_eq!(skills.len(), 3);
        assert_eq!(skills[0].1, PathBuf::from("/root/skills"));
        assert!(skills.iter().all(|(_, _, writable)| !writable), "{skills:?}");
        assert!(skills.iter().any(|(_, c, _)| c == Path::new("/root/skills/shared")));
        assert!(skills.iter().any(|(_, c, _)| c == Path::new("/root/skills/daniele")));
    }
}
