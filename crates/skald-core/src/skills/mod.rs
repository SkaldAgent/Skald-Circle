//! The skills index — **the only thing about a skill that reaches the prompt**.
//!
//! A skill is a folder with a `SKILL.md` in it, living in one of the two trees
//! `UserFs` mounts read-only (`skills/shared/<id>` and `skills/<username>/<id>`,
//! see [`core_api::user_fs::SkillMounts`]). Nothing here is a manager in the usual
//! sense: this module is a set of **pure functions over those two paths**, in the
//! shape of `LlmCommandManager` and for the same reason — the list must be a
//! function of the content, never a file someone maintains by hand, or it diverges
//! at the first skill added.
//!
//! What reaches the model is deliberately thin (blueprint §5, progressive
//! disclosure): the **path** of each `SKILL.md` plus a truncated `description`.
//! The body is never injected — the model reads it with `read_file` when the
//! description matches. Carrying the full path rather than an id plus a
//! composition rule is what makes a dedicated read tool unnecessary: it costs a
//! few tokens per line and removes the one step the model can get wrong on its own.
//!
//! Three properties are load-bearing, and each is here because the alternative
//! fails in a specific way:
//!
//! - **A stable order** (scope, then id). The index sits inside the string every
//!   provider uses as its cache key, so a non-deterministic order would cost a
//!   miss on every rebuild.
//! - **A deterministic cut at the budget**, closed by an explicit omission line.
//!   Without the determinism the stable order stops buying a stable cache key;
//!   without the line the model reads a truncated list *believing it complete* and
//!   concludes in good faith that a skill does not exist.
//! - **A broken skill is skipped, never fatal.** The index is built while
//!   assembling a system prompt; one malformed frontmatter must not take a
//!   conversation down with it.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use core_api::user_fs::{SKILLS_ROOT, SKILLS_SHARED_SCOPE, UserFs};
use tracing::warn;

pub mod install;
pub mod inventory;
pub mod validate;
pub mod watch;

/// The file that makes a directory a skill.
pub const SKILL_FILE: &str = "SKILL.md";

/// Which of a user's two trees an operation addresses.
///
/// The tools take `"mine"` / `"global"` rather than a username, and that is
/// deliberate (blueprint §4.1): a tool argument naming the caller invites
/// passing somebody *else's* name, which the server would then have to ignore.
/// Human-readable path, stable argument for the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `skills/{username}/…` — the caller's own.
    Own,
    /// `skills/shared/…` — the group's, gated by the `skill.manage` capability.
    Shared,
}

impl Scope {
    pub fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw {
            "mine"   => Ok(Scope::Own),
            "global" => Ok(Scope::Shared),
            other    => anyhow::bail!(
                "unknown scope `{other}`: use \"mine\" (your own skills) or \"global\" \
                 (the whole group's)"
            ),
        }
    }

    /// The spelling the tools take and the model reads back.
    pub fn as_arg(self) -> &'static str {
        match self {
            Scope::Own    => "mine",
            Scope::Shared => "global",
        }
    }
}

// ── Prompt freshness (blueprint §6) ──────────────────────────────────────────

/// Whose system prompts a skills change has made stale.
///
/// Distinct from [`Scope`], which says *where a write went*: a write to the
/// group's tree makes every live member's prompt stale, a write to one member's
/// own tree makes only theirs. Session 5's `SkillsChanged` event carries the
/// same shape, for the same reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptScope {
    /// The group's tree changed: everyone's index moved.
    Everyone,
    /// One member's own tree changed.
    User(String),
}

/// How a skills write reaches conversations that are already running.
///
/// The index sits inside the frozen system prefix, which is rebuilt only after
/// twenty idle minutes ([`crate::loop_adapters::prefix_cache`]). That is right
/// for a file edited underneath a running conversation and wrong here: an admin
/// who installs a skill and then asks the assistant to use it must not be told
/// for twenty minutes that it does not exist.
///
/// **Not on the system bus.** `SkillsChanged` (session 5) is for a change made
/// *outside* the process, by someone editing files on the box; a write made by a
/// tool is already inside the process and can say so directly, which is both
/// immediate and impossible to lose. The bus stays for the case it was designed
/// for.
#[async_trait::async_trait]
pub trait PromptPrefixes: Send + Sync {
    async fn invalidate(&self, scope: PromptScope);
}

/// The cell the skill tools hold, filled once the instance exists.
///
/// The tools are built during composition, before `Skald` does; the reactor they
/// need can therefore only be installed afterwards — the same post-construction
/// shape as the plugin manager's `set_skald` and the user-lifecycle reconciler.
/// An empty cell is a silent no-op rather than an error: the only way to reach
/// it is a `Skald` that failed to finish building, in which case there are no
/// live conversations to keep fresh either.
#[derive(Default)]
pub struct PromptPrefixCell(OnceLock<Arc<dyn PromptPrefixes>>);

impl PromptPrefixCell {
    pub fn install(&self, sink: Arc<dyn PromptPrefixes>) {
        let _ = self.0.set(sink);
    }

    pub async fn invalidate(&self, scope: PromptScope) {
        if let Some(sink) = self.0.get() {
            sink.invalidate(scope).await;
        }
    }
}

/// How much of a `description` the index carries. The full text lives on disk and
/// is surfaced by the enumeration tool; this cap applies **only** to the index,
/// which is the one place tokens are paid on every request of every user.
///
/// Hermes cuts at 60. Sixty is too few here: the `description` *is* the use
/// condition ("when should I reach for this?"), and cutting mid-sentence removes
/// exactly the part that decides the trigger.
pub const DESCRIPTION_LIMIT: usize = 200;

/// Ceiling on the whole rendered index, in bytes. A badly written skill must not
/// be able to eat the prompt of everybody in the house.
pub const INDEX_BUDGET: usize = 8 * 1024;

/// Room kept aside for the omission line, so appending it can never push the
/// render past [`INDEX_BUDGET`]. A fixed reserve rather than a computed one keeps
/// the cut a pure function of the ordered list.
const OMISSION_RESERVE: usize = 96;

/// One installed skill, as the index sees it: where it is and when to reach for
/// it. The body never enters this type — it is read from disk, by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The directory name, which **is** the id.
    pub id:          String,
    /// The agent-visible scope segment: `shared`, or the owner's username.
    pub scope:       String,
    /// The frontmatter `description`, verbatim and untruncated.
    pub description: String,
}

impl Skill {
    /// The skill's folder, in agent vocabulary (`skills/shared/ics-import`).
    pub fn agent_dir(&self) -> String {
        format!("{SKILLS_ROOT}/{}/{}", self.scope, self.id)
    }

    /// The path the index prints — the `SKILL.md` itself, ready for `read_file`.
    pub fn skill_file(&self) -> String {
        format!("{}/{SKILL_FILE}", self.agent_dir())
    }
}

/// Every skill visible to this user, in the index's stable order: the group's
/// tree first, then their own, each sorted by id.
///
/// Per-user by construction — the own tree comes from `fs.skills`, which is built
/// for one member — so another member's private skills cannot appear here. A
/// `UserFs` without the skills tree (an inert placeholder, a unit test) simply has
/// none.
pub fn list(fs: &UserFs) -> Vec<Skill> {
    let Some(sk) = &fs.skills else { return Vec::new() };
    let mut out = Vec::new();
    collect(SKILLS_SHARED_SCOPE, &sk.shared_host, &mut out);
    collect(&sk.own_username, &sk.own_host, &mut out);
    out
}

/// Reads one scope's tree, appending its valid skills in id order. A tree that
/// does not exist yet is not an error: it is the state of every fresh instance.
fn collect(scope: &str, host: &Path, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(host) else { return };

    let mut found: Vec<Skill> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(id) = path.file_name().and_then(|n| n.to_str()) else { continue };
        // Dot-directories are plumbing (a staging leftover, an editor's cruft),
        // never a skill: an id starting with a dot cannot be registered.
        if id.starts_with('.') {
            continue;
        }

        let body = match std::fs::read_to_string(path.join(SKILL_FILE)) {
            Ok(b) => b,
            Err(e) => {
                warn!(scope, skill = id, error = %e, "skill skipped: no readable SKILL.md");
                continue;
            }
        };
        let front = match parse_front_matter(&body) {
            Ok(f) => f,
            Err(problem) => {
                warn!(scope, skill = id, problem, "skill skipped: invalid frontmatter");
                continue;
            }
        };
        // The id is the directory, always — that is what every path in the index
        // is built from. A `name` that disagrees is worth saying out loud (the
        // registration tool makes the two agree by construction, so this can only
        // be a folder placed by hand) but not worth hiding the skill over.
        if front.name != id {
            warn!(
                scope, skill = id, declared = front.name,
                "skill frontmatter `name` differs from its directory; the directory wins"
            );
        }
        found.push(Skill { id: id.to_string(), scope: scope.to_string(), description: front.description });
    }

    found.sort_by(|a, b| a.id.cmp(&b.id));
    out.extend(found);
}

/// The two mandatory frontmatter fields. Unknown keys (`license`,
/// `allowed-tools`, anything a skill written elsewhere carries) are ignored.
#[derive(serde::Deserialize)]
pub(crate) struct FrontMatter {
    #[serde(default)]
    pub(crate) name:        String,
    #[serde(default)]
    pub(crate) description: String,
}

/// Parses the leading `---` YAML block of a `SKILL.md`. `Err` carries a short
/// reason, which the caller logs — this is the one place a hand-edited skill goes
/// wrong, so the log line has to say which of the three ways it did.
///
/// Shared with [`validate`] on purpose: two frontmatter parsers would agree on
/// the easy cases and diverge on the first odd one, and the pair that must never
/// disagree is exactly *what the registration accepts* and *what the index
/// shows* — a skill installed but invisible is the worst of both.
pub(crate) fn parse_front_matter(body: &str) -> Result<FrontMatter, &'static str> {
    let rest = body
        .strip_prefix("---\n")
        .or_else(|| body.strip_prefix("---\r\n"))
        .ok_or("no frontmatter block")?;
    let end = rest
        .split_inclusive('\n')
        .scan(0usize, |at, line| {
            let start = *at;
            *at += line.len();
            Some((start, line))
        })
        .find(|(_, line)| matches!(line.trim_end(), "---" | "..."))
        .map(|(start, _)| start)
        .ok_or("unterminated frontmatter block")?;

    let front: FrontMatter = serde_yaml::from_str(&rest[..end]).map_err(|_| "not valid YAML")?;
    if front.name.trim().is_empty() {
        return Err("frontmatter has no `name`");
    }
    if front.description.trim().is_empty() {
        return Err("frontmatter has no `description`");
    }
    Ok(FrontMatter { name: front.name.trim().to_string(), description: front.description.trim().to_string() })
}

/// The index for one user, ready to replace `__SKILLS_LIST__`.
pub fn render_index(fs: &UserFs) -> String {
    render(&list(fs))
}

/// A stable digest of a rendered index. The invalidation rule of blueprint §6 is
/// keyed on **this string changing**, not on a file inside a skill changing:
/// editing a script or a reference document leaves the index byte-identical, so it
/// costs nobody a cache miss. Only adding, removing or re-describing a skill does.
pub fn digest(rendered: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(rendered.as_bytes()))
}

/// A digest of one scope **tree's** visible content — the sorted
/// (id, description) pairs, which is everything the index ever prints from it.
///
/// This is the file-watcher's gate (blueprint §8.2), and the rule is §6's, per
/// tree instead of per render: editing a script or a reference document leaves
/// it alone; adding, removing or re-describing a skill moves it. A collision
/// marker needs no hashing of its own — it is a function of the two id sets,
/// and those are hashed. An empty or missing tree digests as [`digest`]`("")`.
pub fn tree_digest(host: &Path) -> String {
    use sha2::{Digest, Sha256};
    let label = host.file_name().and_then(|n| n.to_str()).unwrap_or("?");
    let mut found = Vec::new();
    collect(label, host, &mut found);
    let mut hasher = Sha256::new();
    for s in &found {
        hasher.update(s.id.as_bytes());
        hasher.update([0]);
        hasher.update(s.description.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

/// The imperative preamble. Deliberately pushy — the failure mode of every skill
/// system is the model *under*-triggering, and a neutral "the following skills are
/// available" produces a model that scrolls past them.
const HEADER: &str = "\
## Skills (mandatory)

Before replying, scan the skills below. If a skill matches or is even partially \
relevant to your task, you MUST read its `SKILL.md` with `read_file` and follow \
its instructions. Err on the side of reading it — it is always better to have \
context you don't need than to miss critical steps, pitfalls or established \
workflows. Skills encode how a task should be done here, so read one even for a \
task you already know how to do.

<available_skills>
";

/// Closing rules. The `workdir` sentence is here, said once, because there is no
/// launch tool to hide it in: a skill's scripts are run with the general-purpose
/// `execute_cmd`, and a model that runs one from the home gets a bare ENOENT.
const FOOTER: &str = "\
</available_skills>

Only proceed without reading a skill if genuinely none are relevant.

Run a skill's scripts with `execute_cmd`, setting `workdir` to the skill's own \
folder. The whole `skills/` tree is read-only: anything a skill needs to write \
(caches, state, dependencies) goes in your home or `/tmp`.";

/// Renders the index from an ordered skill list — pure, so the budget cut and the
/// collision marking are testable without a filesystem.
///
/// **Empty in, empty out**, and that is a contract rather than an optimisation:
/// every word of prose lives in here, so an instance with no skills spends nothing
/// and leaves no orphan sentence from which the model could infer that something
/// exists. (The MCP list is the counter-example — its prose sits *around* the
/// placeholder, so an empty list left a promise of a table behind, and the model
/// answered by inventing a discovery tool.)
pub fn render(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    // An id present in both trees is marked on **both** lines: neither wins in
    // silence. The personal one winning would be a quiet divergence from the
    // group's set, the group's one winning would ignore the member's own work.
    // With full paths in the index the disambiguation is already free — the two
    // lines differ — so fail-loud costs only the marker.
    let colliding: std::collections::HashSet<&str> = skills
        .iter()
        .filter(|s| skills.iter().any(|o| o.id == s.id && o.scope != s.scope))
        .map(|s| s.id.as_str())
        .collect();

    let paths: Vec<String> = skills.iter().map(Skill::skill_file).collect();
    let width = paths.iter().map(String::len).max().unwrap_or(0);

    let rows: Vec<String> = skills
        .iter()
        .zip(&paths)
        .map(|(s, path)| {
            let mut desc = truncate(&flatten(&s.description), DESCRIPTION_LIMIT);
            if colliding.contains(s.id.as_str()) {
                desc.push_str("  [name collision]");
            }
            format!("  {path:<width$}  {desc}\n")
        })
        .collect();

    let mut out = String::with_capacity(HEADER.len() + FOOTER.len() + 128);
    out.push_str(HEADER);

    let mut room = INDEX_BUDGET
        .saturating_sub(HEADER.len() + FOOTER.len() + OMISSION_RESERVE);
    let mut omitted = 0;
    for (i, row) in rows.iter().enumerate() {
        // Stop at the **first** row that does not fit, rather than skipping it and
        // trying the next: the cut has to be a suffix of the stable order, or two
        // renders of the same set could keep different skills.
        if row.len() > room {
            omitted = rows.len() - i;
            break;
        }
        room -= row.len();
        out.push_str(row);
    }
    if omitted > 0 {
        warn!(omitted, total = rows.len(), "skills index over budget: tail omitted");
        out.push_str(&format!("  [{omitted} more skills omitted — index budget reached]\n"));
    }

    out.push_str(FOOTER);
    out
}

/// A description as one line: a multi-line one would break the column layout, and
/// the index is read as a table.
fn flatten(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncates to `limit` **characters** (never bytes — this text is user-authored
/// and routinely accented), marking the cut so the model knows it read a prefix.
fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let mut out: String = s.chars().take(limit).collect();
    out.push('…');
    out
}

/// A temporary `{WD}` with the two scope trees and a `UserFs` over it — shared
/// by the index, install and inventory tests, which all need the same fixture
/// and would otherwise each grow their own slightly different one.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use core_api::user_fs::SkillMounts;
    use std::path::PathBuf;

    pub(crate) struct Tree {
        pub(crate) root: PathBuf,
        pub(crate) fs:   UserFs,
    }

    impl Tree {
        pub(crate) fn new(tag: &str, username: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "skald-skills-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            let shared = root.join("skills");
            let own = root.join("skills-users").join("u1");
            std::fs::create_dir_all(&shared).unwrap();
            std::fs::create_dir_all(&own).unwrap();
            let fs = UserFs::new(
                "u1",
                root.join("homes").join("u1"),
                "skald-u1",
                PathBuf::from("/root"),
                vec![],
                vec![],
                None,
            )
            .with_skills(SkillMounts {
                root_host:    root.join(".skills-root").join("u1"),
                shared_host:  shared,
                own_host:     own,
                own_username: username.into(),
            });
            Self { root, fs }
        }

        /// Drops a skill straight into a scope tree, the way a hand-placed folder
        /// on the box arrives — bypassing the registration tool, which these
        /// tests are deliberately not exercising.
        pub(crate) fn write(&self, scope: &str, id: &str, body: &str) {
            let base = match scope {
                "shared" => self.root.join("skills"),
                _        => self.root.join("skills-users").join("u1"),
            };
            let dir = base.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(SKILL_FILE), body).unwrap();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    pub(crate) fn valid(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\nThe body.\n")
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::{Tree, valid};
    use super::*;
    use std::path::PathBuf;

    fn skill(scope: &str, id: &str, description: &str) -> Skill {
        Skill { id: id.into(), scope: scope.into(), description: description.into() }
    }

    /// Both trees are enumerated, the group's first, each in id order — and the
    /// order is the whole reason: it is inside the provider's cache key.
    #[test]
    fn both_scopes_are_listed_in_a_stable_order() {
        let t = Tree::new("order", "daniele");
        t.write("shared", "pdf-forms", &valid("pdf-forms", "Fill a PDF form."));
        t.write("shared", "ics-import", &valid("ics-import", "Import an ICS feed."));
        t.write("mine", "spesa", &valid("spesa", "Reconcile the statement."));

        let got: Vec<(String, String)> =
            list(&t.fs).into_iter().map(|s| (s.scope, s.id)).collect();
        assert_eq!(
            got,
            vec![
                ("shared".into(), "ics-import".into()),
                ("shared".into(), "pdf-forms".into()),
                ("daniele".into(), "spesa".into()),
            ]
        );
    }

    /// A skill nobody can parse is skipped; the ones around it are not. The index
    /// is built while assembling a prompt, so a broken folder must cost its own
    /// line and nothing else.
    #[test]
    fn a_malformed_skill_is_skipped_not_fatal() {
        let t = Tree::new("malformed", "daniele");
        t.write("shared", "good", &valid("good", "Works."));
        t.write("shared", "no-frontmatter", "Just a body, no YAML at all.\n");
        t.write("shared", "unterminated", "---\nname: x\ndescription: y\n");
        t.write("shared", "not-yaml", "---\nname: [unclosed\n---\n");
        t.write("shared", "no-description", "---\nname: x\n---\n");
        std::fs::create_dir_all(t.root.join("skills").join("no-skill-md")).unwrap();

        let ids: Vec<String> = list(&t.fs).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["good".to_string()]);
    }

    /// The directory is the id, whatever the frontmatter says — every path in the
    /// index is built from it.
    #[test]
    fn the_directory_is_the_id() {
        let t = Tree::new("id", "daniele");
        t.write("shared", "ics-import", &valid("something-else", "Import an ICS feed."));
        let got = list(&t.fs);
        assert_eq!(got[0].id, "ics-import");
        assert_eq!(got[0].skill_file(), "skills/shared/ics-import/SKILL.md");
    }

    /// The heart of the multi-user half: the own tree is keyed on the userid, so a
    /// private skill of one member cannot reach another member's prompt.
    #[test]
    fn a_private_skill_belongs_to_one_member_only() {
        let a = Tree::new("private-a", "anna");
        a.write("mine", "budget", &valid("budget", "Anna's own."));
        let b = Tree::new("private-b", "bruno");

        assert!(render_index(&a.fs).contains("skills/anna/budget/SKILL.md"));
        assert_eq!(render_index(&b.fs), "");
    }

    /// Nothing installed ⇒ nothing rendered. Not "an empty section": the whole
    /// prose lives inside the render precisely so that this case costs zero tokens
    /// and leaves no sentence the model could read as a promise.
    #[test]
    fn no_skills_renders_nothing_at_all() {
        assert_eq!(render(&[]), "");
        let bare = UserFs::new("u1", PathBuf::from("/wd/homes/u1"), "c", PathBuf::from("/root"), vec![], vec![], None);
        assert_eq!(render_index(&bare), "");
    }

    #[test]
    fn a_line_carries_the_full_path_and_a_truncated_description() {
        let long = "x".repeat(DESCRIPTION_LIMIT + 50);
        let out = render(&[
            skill("shared", "ics-import", "Download an iCalendar (ICS) feed\nand output JSON."),
            skill("daniele", "spesa", &long),
        ]);

        // The path is printed in full: no id-plus-composition-rule for the model
        // to get wrong.
        assert!(out.contains("skills/shared/ics-import/SKILL.md"), "{out}");
        assert!(out.contains("skills/daniele/spesa/SKILL.md"), "{out}");
        // A multi-line description becomes one line.
        assert!(out.contains("Download an iCalendar (ICS) feed and output JSON."), "{out}");
        // …and a long one is cut, visibly.
        assert!(out.contains(&format!("{}…", "x".repeat(DESCRIPTION_LIMIT))), "{out}");
        assert!(!out.contains(&"x".repeat(DESCRIPTION_LIMIT + 1)), "{out}");
        // The imperative header and the closing rule are both there.
        assert!(out.starts_with("## Skills (mandatory)"), "{out}");
        assert!(out.contains("MUST read its `SKILL.md`"), "{out}");
        assert!(out.ends_with("goes in your home or `/tmp`."), "{out}");
    }

    /// The same id in both trees: both lines stay, both are marked. Neither tree
    /// shadows the other, here or in the path router.
    #[test]
    fn a_colliding_id_is_marked_on_both_lines() {
        let out = render(&[
            skill("shared", "ics-import", "The group's."),
            skill("shared", "pdf-forms", "Untouched."),
            skill("daniele", "ics-import", "My fork."),
        ]);
        assert_eq!(out.matches("[name collision]").count(), 2, "{out}");
        for line in out.lines().filter(|l| l.contains("pdf-forms")) {
            assert!(!line.contains("[name collision]"), "{line}");
        }
    }

    /// Over budget the index cuts from the tail of the stable order and says how
    /// many it dropped — deterministically, because the cut is part of the cache
    /// key, and out loud, because a silently truncated index makes the model
    /// conclude in good faith that a skill does not exist.
    #[test]
    fn over_budget_the_tail_is_cut_deterministically_and_announced() {
        let many: Vec<Skill> = (0..400)
            .map(|i| skill("shared", &format!("skill-{i:03}"), &"d".repeat(DESCRIPTION_LIMIT)))
            .collect();

        let out = render(&many);
        assert!(out.len() <= INDEX_BUDGET, "budget blown: {} bytes", out.len());
        assert!(out.contains("more skills omitted — index budget reached"), "{out}");
        // A suffix of the order is what went missing: the first is in, the last is not.
        assert!(out.contains("skills/shared/skill-000/SKILL.md"), "{out}");
        assert!(!out.contains("skills/shared/skill-399/SKILL.md"), "{out}");
        // Same set in, same bytes out — otherwise the stable order buys nothing.
        assert_eq!(out, render(&many));
        assert_eq!(digest(&out), digest(&render(&many)));
    }

    /// The digest keys on what the model can see. Editing a script or a reference
    /// document leaves it alone; re-describing a skill moves it.
    #[test]
    fn the_digest_follows_the_index_not_the_files() {
        let before = render(&[skill("shared", "ics-import", "Import an ICS feed.")]);
        let same   = render(&[skill("shared", "ics-import", "Import an ICS feed.")]);
        let after  = render(&[skill("shared", "ics-import", "Import an ICS feed, then dedupe.")]);
        assert_eq!(digest(&before), digest(&same));
        assert_ne!(digest(&before), digest(&after));
    }
}

