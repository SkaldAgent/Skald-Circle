//! The skills freshness watcher (blueprint §8.2): freshness for edits made **by
//! hand on the box**.
//!
//! Every in-process writer — `skill_register`, `skill_delete`, the future UI —
//! already invalidates the prompt prefix directly; this task exists for the one
//! writer that is not in the process: the admin in SSH, a `git pull` of skills.
//! It is deliberately *not* on the correctness path: a missed event costs a
//! stale index for the twenty minutes of the prefix TTL, never a wrong one.
//!
//! The gate is the digest, and it is the whole design. `notify` is noisy — an
//! editor's save is a burst, an install is dozens of events, and every prompt
//! build *reads* every `SKILL.md` — so what reaches the bus is never "the fs
//! moved" but "the rendered index would differ" ([`super::tree_digest`]). A
//! script edit produces events and then silence, which is exactly the property
//! §6 asks for: the frozen prefix citing that skill has not aged.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use core_api::system_bus::{SkillScope, SystemEvent, SystemEventBus};
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::container::{SKILLS_DIR, SKILLS_USERS_DIR};

/// The quiet period after the last fs event before the digests are recomputed.
/// What matters is the settled state of the tree, not any intermediate one.
const DEBOUNCE: Duration = Duration::from_millis(800);

/// Spawns the watcher on the two trees (`{WD}/skills`, `{WD}/skills-users`),
/// emitting `SystemEvent::SkillsChanged` for each scope whose digest moved.
pub fn spawn(bus: Arc<SystemEventBus>, shutdown: CancellationToken) -> tokio::task::JoinHandle<()> {
    let wd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    tokio::spawn(run(bus, shutdown, wd, DEBOUNCE))
}

/// The watcher's body, split from [`spawn`] so the tests can point it at a
/// temporary `{WD}` and shorten the debounce.
async fn run(bus: Arc<SystemEventBus>, shutdown: CancellationToken, wd: PathBuf, debounce: Duration) {
    // The fs backend reports **canonical** paths (on macOS `/var` is a symlink
    // to `/private/var`, and FSEvents answers with the real one), so the roots
    // must be canonical too or `classify` never matches and every event is
    // silently dropped.
    let wd = match wd.canonicalize() {
        Ok(w) => w,
        Err(e) => {
            warn!(path = %wd.display(), error = %e, "skills-watch: cannot canonicalize the working directory, not started");
            return;
        }
    };
    let shared_dir = wd.join(SKILLS_DIR);
    let users_dir = wd.join(SKILLS_USERS_DIR);
    // Created rather than merely watched: these are instance data dirs that
    // `ContainerManager::ensure` would create anyway, and on a box before its
    // first user neither exists yet — a watcher that fails to install at boot
    // would never notice the first tree appearing.
    for dir in [&shared_dir, &users_dir] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!(path = %dir.display(), error = %e, "skills-watch: cannot create the tree, not started");
            return;
        }
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
    let mut watcher = match RecommendedWatcher::new(
        move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            // A pure read is never a change — and it matters here: every prompt
            // build reads every `SKILL.md`, so IN_ACCESS / CLOSE_NOWRITE would
            // re-digest the trees after every single conversation turn.
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            if event.paths.is_empty() {
                return;
            }
            let _ = tx.send(event.paths);
        },
        Config::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            warn!(error = %e, "skills-watch: watcher create failed, not started");
            return;
        }
    };
    for dir in [&shared_dir, &users_dir] {
        if let Err(e) = watcher.watch(dir, RecursiveMode::Recursive) {
            warn!(path = %dir.display(), error = %e, "skills-watch: watch install failed, not started");
            return;
        }
    }
    info!("skills-watch: watching the two skills trees");

    // Baselines, taken before anything can be emitted: only a *change* from
    // here on is worth an announcement. `empty` is the digest of a tree with no
    // visible skill — a tree first seen in that state (e.g. created empty by
    // `ensure` at container setup) alters no index and announces nothing.
    let empty = super::tree_digest(&shared_dir.join("__never__"));
    let mut shared_digest = super::tree_digest(&shared_dir);
    let mut user_digests = digests_by_user(&users_dir);

    let mut touched_shared = false;
    let mut touched_users: HashSet<String> = HashSet::new();
    let mut quiet: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            paths = rx.recv() => {
                let Some(paths) = paths else { break }; // watcher dropped
                for p in &paths {
                    match classify(p, &shared_dir, &users_dir) {
                        Some(SkillScope::Global)  => touched_shared = true,
                        Some(SkillScope::User(u)) => { touched_users.insert(u); }
                        None                      => {}
                    }
                }
                // Restart the quiet period on every event: a save burst or an
                // install settles only when the events stop coming.
                quiet = Some(Box::pin(tokio::time::sleep(debounce)));
            }
            // `pending` when disarmed: the arm below fires only once armed.
            _ = async { match &mut quiet { Some(s) => s.as_mut().await, None => std::future::pending().await } } => {
                quiet = None;

                if touched_shared {
                    touched_shared = false;
                    let now = super::tree_digest(&shared_dir);
                    if now != shared_digest {
                        shared_digest = now;
                        info!("skills-watch: the group's tree changed, announcing");
                        bus.send(SystemEvent::SkillsChanged { scope: SkillScope::Global });
                    }
                }
                for uid in touched_users.drain() {
                    let now = super::tree_digest(&users_dir.join(&uid));
                    let old = user_digests.insert(uid.clone(), now.clone());
                    let changed = match old {
                        Some(old) => old != now,
                        None      => now != empty,
                    };
                    if changed {
                        info!(user = %uid, "skills-watch: a member's tree changed, announcing");
                        bus.send(SystemEvent::SkillsChanged { scope: SkillScope::User(uid) });
                    }
                }
            }
        }
    }

    info!("skills-watch: stopped");
}

/// Maps a changed fs path to the scope tree it touches.
///
/// `Path::starts_with` is component-wise, which is exactly what saves the
/// prefix trap here: `{WD}/skills-users/…` does **not** start with
/// `{WD}/skills`. An event on the `skills-users` root itself names no user
/// yet and is ignored — a new member's directory reports itself by its own
/// path.
fn classify(path: &Path, shared_dir: &Path, users_dir: &Path) -> Option<SkillScope> {
    if path.starts_with(shared_dir) {
        return Some(SkillScope::Global);
    }
    let rel = path.strip_prefix(users_dir).ok()?;
    let uid = rel.components().next()?.as_os_str().to_str()?;
    Some(SkillScope::User(uid.to_string()))
}

/// The baseline digests of every member tree present at startup.
fn digests_by_user(users_dir: &Path) -> HashMap<String, String> {
    let Ok(entries) = std::fs::read_dir(users_dir) else { return HashMap::new() };
    entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .map(|uid| {
            let d = super::tree_digest(&users_dir.join(&uid));
            (uid, d)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::tests_support::{Tree, valid};

    /// The classification trap the whole design leans on: `skills-users` is a
    /// **sibling** of `skills`, and a component-wise prefix check must not let
    /// a member's edit fall into the group's scope.
    #[test]
    fn classify_never_confuses_the_two_sibling_trees() {
        let wd = Path::new("/wd");
        let shared = wd.join(SKILLS_DIR);
        let users = wd.join(SKILLS_USERS_DIR);

        assert_eq!(
            classify(Path::new("/wd/skills/ics-import/SKILL.md"), &shared, &users),
            Some(SkillScope::Global)
        );
        assert_eq!(classify(Path::new("/wd/skills"), &shared, &users), Some(SkillScope::Global));
        assert_eq!(
            classify(Path::new("/wd/skills-users/u1/spesa/SKILL.md"), &shared, &users),
            Some(SkillScope::User("u1".into()))
        );
        assert_eq!(
            classify(Path::new("/wd/skills-users/u1"), &shared, &users),
            Some(SkillScope::User("u1".into()))
        );
        // The users root itself names nobody.
        assert_eq!(classify(Path::new("/wd/skills-users"), &shared, &users), None);
        // Anything else is not ours at all.
        assert_eq!(classify(Path::new("/wd/homes/u1/x"), &shared, &users), None);
    }

    /// The gate itself, over a real tree: an edit the index cannot see passes
    /// in silence; one it can see announces. (The pure half —
    /// `the_digest_follows_the_index_not_the_files` — lives in `skills/mod.rs`;
    /// this is the watcher half §15 asks for.)
    #[test]
    fn tree_digest_gates_on_what_the_index_can_see() {
        let t = Tree::new("watch-digest", "daniele");
        t.write("shared", "ics-import", &valid("ics-import", "Import an ICS feed."));
        let dir = t.root.join(SKILLS_DIR);

        let before = super::super::tree_digest(&dir);
        // A script appears: events fire, the index does not move.
        std::fs::write(dir.join("ics-import").join("run.py"), "print('hi')\n").unwrap();
        assert_eq!(super::super::tree_digest(&dir), before);
        // The body changes under the same description: still invisible.
        t.write("shared", "ics-import", &valid("ics-import", "Import an ICS feed."));
        assert_eq!(super::super::tree_digest(&dir), before);
        // A re-description is what the prefix is made of: the digest moves.
        t.write("shared", "ics-import", &valid("ics-import", "Import an ICS feed, then dedupe."));
        assert_ne!(super::super::tree_digest(&dir), before);
        // An empty or missing tree is the same digest as no tree.
        assert_eq!(super::super::tree_digest(&dir.join("__never__")), super::super::digest(""));
    }

    /// End to end: a hand edit on the box reaches the bus — but only when the
    /// index would notice.
    #[tokio::test]
    async fn a_hand_edit_announces_only_what_the_index_feels() {
        let t = Tree::new("watch-e2e", "daniele");
        t.write("shared", "ics-import", &valid("ics-import", "Import an ICS feed."));
        t.write("mine", "spesa", &valid("spesa", "Reconcile the statement."));

        let bus = Arc::new(SystemEventBus::new());
        let mut rx = bus.subscribe();
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run(
            Arc::clone(&bus),
            shutdown.clone(),
            t.root.clone(),
            Duration::from_millis(100),
        ));

        // Give the watcher a moment to install before touching the tree.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // A script edit is fs activity the index cannot see: no announcement,
        // however long we wait.
        std::fs::write(t.root.join(SKILLS_DIR).join("ics-import").join("run.py"), "print(1)\n").unwrap();
        let quiet = tokio::time::timeout(Duration::from_millis(1500), rx.recv()).await;
        assert!(quiet.is_err(), "a script edit announced something: {quiet:?}");

        // A re-description of a shared skill announces the group's scope.
        t.write("shared", "ics-import", &valid("ics-import", "Import an ICS feed, then dedupe."));
        let announced = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("no SkillsChanged within 10s of a description edit")
            .expect("bus closed");
        assert!(
            matches!(announced, SystemEvent::SkillsChanged { scope: SkillScope::Global }),
            "expected SkillsChanged(Global), got {announced:?}"
        );

        // And one of an own skill announces only that member.
        t.write("mine", "spesa", &valid("spesa", "Reconcile the statement, monthly."));
        let announced = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("no SkillsChanged within 10s of an own-skill edit")
            .expect("bus closed");
        assert!(
            matches!(announced, SystemEvent::SkillsChanged { scope: SkillScope::User(ref u) } if u == "u1"),
            "expected SkillsChanged(User(u1)), got {announced:?}"
        );

        shutdown.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
    }
}
