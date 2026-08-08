//! The one door into the two read-only trees (blueprint §7.3/§9).
//!
//! Everything here exists because a skill is an **immutable, validated
//! artefact**: it is copied in whole or not at all, it is never edited in place,
//! and modifying one means registering it again. The tree therefore never holds
//! a half-written skill, which is what lets the index (`super::list`) read it
//! without tolerating intermediate states.
//!
//! Two mechanics carry that promise and neither is optional:
//!
//! - **Staging plus a rename**, never a copy in place, so the indexer cannot
//!   observe a directory being filled.
//! - **Three steps on replacement**, because `rename` over a non-empty directory
//!   fails: move the old one aside, move the new one in, delete the old. Not
//!   atomic in the strict sense — but the uncovered window contains only a state
//!   where the id *does not exist*, never one where it exists half-written, and
//!   that is the property the indexer actually needs.

use std::path::Path;

use anyhow::{Context, Result, bail};
use core_api::user_fs::UserFs;
use serde::{Deserialize, Serialize};

use super::validate::{ValidSkill, validate_dir};
use super::{SKILL_FILE, Scope};

/// The provenance ticket a fetched skill carries, written next to its files.
///
/// The seam between two tools that deliberately know nothing about each other:
/// `fetch_repo` (blueprint §7.5, session 4) downloads without knowing what it
/// downloaded, `skill_register` installs without knowing where it came from, and
/// this file is what crosses between them. `git clone` leaves nothing traceable,
/// so without it neither "where is this skill from?" nor "has it changed
/// upstream?" has an answer later.
///
/// It is *pinning*, not authenticity — whoever serves the repository serves the
/// commit too — the same honesty the marketplace states about its digests.
pub const SOURCE_FILE: &str = ".source.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub url:          String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_path:     Option<String>,
    /// The commit the files were taken from — the field that makes a later
    /// upstream change *detectable*.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit:       Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at:   Option<String>,
    /// Stamped by [`install`], so the ticket answers "since when is this here?"
    /// as well as "where from?".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
}

/// Reads the ticket a source folder carries, if it carries one. A malformed one
/// is *ignored*, never fatal: provenance is metadata about the skill, and losing
/// it must not stop an otherwise valid installation.
pub fn read_provenance(dir: &Path) -> Option<Provenance> {
    let raw = std::fs::read_to_string(dir.join(SOURCE_FILE)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// What an installation did, for the tool's answer to the model.
pub struct Installed {
    pub id:        String,
    /// The agent path of the installed folder (`skills/shared/ics-import`).
    pub agent_dir: String,
    /// Whether it took the place of a skill with the same id in the same scope.
    pub replaced:  bool,
}

/// The host directory backing one scope of this user's tree.
pub fn tree_of(fs: &UserFs, scope: Scope) -> Result<&Path> {
    let sk = fs.skills.as_ref().ok_or_else(|| {
        anyhow::anyhow!("skills are not available in this context")
    })?;
    Ok(match scope {
        Scope::Shared => sk.shared_host.as_path(),
        Scope::Own    => sk.own_host.as_path(),
    })
}

/// The agent-visible scope segment (`shared`, or the caller's username).
pub fn segment_of(fs: &UserFs, scope: Scope) -> Result<&str> {
    let sk = fs.skills.as_ref().ok_or_else(|| {
        anyhow::anyhow!("skills are not available in this context")
    })?;
    Ok(match scope {
        Scope::Shared => core_api::user_fs::SKILLS_SHARED_SCOPE,
        Scope::Own    => sk.own_username.as_str(),
    })
}

/// Validates `source` and installs it into `scope`, replacing an existing skill
/// of the same id in that scope.
///
/// The source is copied *before* anything at the destination moves, so
/// registering a skill onto itself (`skill_register("global",
/// "skills/daniele/foo")` — the promotion path, which is deliberately the same
/// call rather than an endpoint of its own) needs no special case.
pub fn install(fs: &UserFs, scope: Scope, source: &Path) -> Result<Installed> {
    let valid = validate_dir(source)?;
    install_validated(fs, scope, source, &valid)
}

fn install_validated(
    fs:     &UserFs,
    scope:  Scope,
    source: &Path,
    valid:  &ValidSkill,
) -> Result<Installed> {
    let tree = tree_of(fs, scope)?.to_path_buf();
    std::fs::create_dir_all(&tree)
        .with_context(|| format!("cannot open the skills tree at {}", tree.display()))?;

    let target = tree.join(&valid.id);
    let replaced = target.exists();
    let tag = uuid::Uuid::new_v4().simple().to_string();
    // Staging lives **inside the destination tree**: `rename` only works within
    // one filesystem, and a dot-directory is skipped by the indexer (see
    // `super::collect`), so a crash mid-copy leaves litter, never a skill.
    let staging = tree.join(format!(".staging-{tag}"));

    let outcome = (|| -> Result<()> {
        copy_tree(source, &staging)?;
        stamp_provenance(source, &staging);
        if replaced {
            let parked = tree.join(format!(".old-{tag}"));
            std::fs::rename(&target, &parked)
                .with_context(|| format!("cannot replace the installed `{}`", valid.id))?;
            // From here the id does not exist. If the second rename fails we put
            // the old one back rather than leave the scope short of a skill it
            // had a moment ago.
            if let Err(e) = std::fs::rename(&staging, &target) {
                let _ = std::fs::rename(&parked, &target);
                return Err(anyhow::Error::new(e)
                    .context(format!("cannot install `{}`", valid.id)));
            }
            let _ = std::fs::remove_dir_all(&parked);
        } else {
            std::fs::rename(&staging, &target)
                .with_context(|| format!("cannot install `{}`", valid.id))?;
        }
        Ok(())
    })();

    if outcome.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    outcome?;

    Ok(Installed {
        id:        valid.id.clone(),
        agent_dir: format!(
            "{}/{}/{}",
            core_api::user_fs::SKILLS_ROOT,
            segment_of(fs, scope)?,
            valid.id
        ),
        replaced,
    })
}

/// Carries the provenance ticket across, stamping the install date. Best-effort
/// for the same reason [`read_provenance`] is tolerant: this is a label on the
/// artefact, not part of it.
fn stamp_provenance(source: &Path, staging: &Path) {
    let Some(mut p) = read_provenance(source) else { return };
    p.installed_at = Some(chrono::Utc::now().to_rfc3339());
    if let Ok(json) = serde_json::to_string_pretty(&p) {
        let _ = std::fs::write(staging.join(SOURCE_FILE), json);
    }
}

/// Copies a validated tree, refusing links again on the way.
///
/// The re-check is not redundant paranoia about the walk in `validate`: the
/// source folder is writable by the caller and by their container, so between
/// the two passes it can change. The cheap answer is to never follow a link at
/// either point.
///
/// Shared with `fetch_repo`, whose staging area is writable by the caller's
/// container for exactly as long and so needs exactly the same guarantee.
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)
        .with_context(|| format!("cannot create {}", to.display()))?;
    for entry in std::fs::read_dir(from)
        .with_context(|| format!("cannot read {}", from.display()))?
    {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let meta = std::fs::symlink_metadata(&src)?;
        if meta.file_type().is_symlink() {
            bail!("`{}` is a symbolic link", entry.file_name().to_string_lossy());
        }
        if meta.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)
                .with_context(|| format!("cannot copy {}", src.display()))?;
        }
    }
    Ok(())
}

/// Removes an installed skill. No recycle bin, deliberately: the source it was
/// registered from almost always still exists, and a bin would be a second tree
/// to index, mount and explain.
pub fn remove(fs: &UserFs, scope: Scope, id: &str) -> Result<()> {
    let tree = tree_of(fs, scope)?;
    // The id names a directory *inside* the tree and nothing else — a `/` or a
    // `..` here would be a path, and paths are not ids.
    if id.is_empty() || id.contains('/') || id.contains('\\') || id.starts_with('.') {
        bail!("`{id}` is not a skill id (it is the folder name, e.g. `ics-import`)");
    }
    let dir = tree.join(id);
    if !dir.is_dir() {
        bail!(
            "no skill `{id}` in {}/{}/",
            core_api::user_fs::SKILLS_ROOT,
            segment_of(fs, scope)?
        );
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("cannot remove `{id}`"))?;
    Ok(())
}

// ── The approval card ─────────────────────────────────────────────────────────

/// What the human is shown before a registration goes through: `(old, new)` for
/// the diff card the write tools already use.
///
/// This is the review moment blueprint §9.1 calls the point of the whole design
/// — for the group's scope it is the **only** time a person reads a text that
/// will enter everybody's prompt — so `new` is the candidate's `SKILL.md` in
/// full, not a summary of it. When the id already exists, `old` is the installed
/// body, which turns the card into a diff of what actually changes.
///
/// `None` when the source cannot be read or does not validate: the card then
/// falls back to the generic approval event, and the refusal comes from the tool
/// itself with its own message.
pub fn preview(fs: &UserFs, scope: Scope, source: &Path) -> Option<(String, Option<String>, String)> {
    let valid = validate_dir(source).ok()?;
    let tree = tree_of(fs, scope).ok()?;
    let target = tree.join(&valid.id);
    let old = std::fs::read_to_string(target.join(SKILL_FILE)).ok();

    let agent_dir = format!(
        "{}/{}/{}",
        core_api::user_fs::SKILLS_ROOT,
        segment_of(fs, scope).ok()?,
        valid.id
    );
    let header = card_header(&valid, scope, old.is_some(), &agent_dir);
    let body = std::fs::read_to_string(source.join(SKILL_FILE)).ok()?;

    Some((
        agent_dir,
        old.map(|o| format!("{}\n{o}", card_header(&valid, scope, true, ""))),
        format!("{header}\n{body}"),
    ))
}

fn card_header(valid: &ValidSkill, scope: Scope, replacing: bool, _dir: &str) -> String {
    let what = if replacing { "REPLACES the installed skill" } else { "new skill" };
    let audience = match scope {
        Scope::Shared => "the whole group — every member reads it as instructions",
        Scope::Own    => "you only",
    };
    let deps = valid.deps().unwrap_or_else(|| "none".into());
    format!(
        "<!-- {what}: `{}` · visible to {audience}\n     \
         {} files, {} KB · scripts: {} · dependency manifest: {deps}\n     \
         files: {} -->\n",
        valid.id,
        valid.files.len(),
        valid.size_bytes.div_ceil(1024),
        if valid.has_scripts() { "yes" } else { "no" },
        valid
            .files
            .iter()
            .map(|f| f.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::tests_support::Tree;

    fn front(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n")
    }

    /// The installed folder is named by the frontmatter, and the index picks it
    /// up straight away.
    #[test]
    fn a_draft_folder_installs_under_its_declared_name() {
        let t = Tree::new("install-name", "daniele");
        let src = t.root.join("homes/u1/draft-2");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(SKILL_FILE), front("ics-import", "Import an ICS feed.")).unwrap();

        let got = install(&t.fs, Scope::Own, &src).unwrap();
        assert_eq!(got.id, "ics-import");
        assert_eq!(got.agent_dir, "skills/daniele/ics-import");
        assert!(!got.replaced);

        let listed = crate::skills::list(&t.fs);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].skill_file(), "skills/daniele/ics-import/SKILL.md");
    }

    /// Re-registering the same id replaces it, and nothing of the old copy
    /// survives — the artefact is whole or absent, never merged.
    #[test]
    fn re_registering_replaces_without_leaving_the_old_files() {
        let t = Tree::new("install-replace", "daniele");
        let src = t.root.join("homes/u1/v1");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(SKILL_FILE), front("x", "First.")).unwrap();
        std::fs::write(src.join("old-helper.py"), "1").unwrap();
        install(&t.fs, Scope::Own, &src).unwrap();

        let src2 = t.root.join("homes/u1/v2");
        std::fs::create_dir_all(&src2).unwrap();
        std::fs::write(src2.join(SKILL_FILE), front("x", "Second.")).unwrap();
        let got = install(&t.fs, Scope::Own, &src2).unwrap();

        assert!(got.replaced);
        let installed = t.root.join("skills-users/u1/x");
        assert!(!installed.join("old-helper.py").exists());
        assert_eq!(crate::skills::list(&t.fs)[0].description, "Second.");
        // No staging or parked leftovers.
        let stray: Vec<_> = std::fs::read_dir(t.root.join("skills-users/u1"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(stray.is_empty(), "leftovers: {stray:?}");
    }

    /// Promotion is the same call with a different scope, and its source is the
    /// already-installed copy — which the copy-first ordering makes safe.
    #[test]
    fn promoting_ones_own_skill_to_the_group_is_the_same_call() {
        let t = Tree::new("install-promote", "daniele");
        let src = t.root.join("homes/u1/draft");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(SKILL_FILE), front("x", "Mine.")).unwrap();
        install(&t.fs, Scope::Own, &src).unwrap();

        let mine = t.root.join("skills-users/u1/x");
        install(&t.fs, Scope::Shared, &mine).unwrap();

        let ids: Vec<(String, String)> = crate::skills::list(&t.fs)
            .into_iter()
            .map(|s| (s.scope, s.id))
            .collect();
        assert_eq!(
            ids,
            vec![("shared".into(), "x".into()), ("daniele".into(), "x".into())]
        );
    }

    /// A refused source leaves the tree exactly as it was — the validation runs
    /// before a single byte is copied.
    #[test]
    fn an_invalid_source_touches_nothing() {
        let t = Tree::new("install-invalid", "daniele");
        let src = t.root.join("homes/u1/broken");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("notes.txt"), "no frontmatter here").unwrap();

        assert!(install(&t.fs, Scope::Own, &src).is_err());
        assert!(crate::skills::list(&t.fs).is_empty());
        assert_eq!(std::fs::read_dir(t.root.join("skills-users/u1")).unwrap().count(), 0);
    }

    #[test]
    fn the_provenance_ticket_crosses_into_the_installed_copy() {
        let t = Tree::new("install-prov", "daniele");
        let src = t.root.join("homes/u1/fetched");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(SKILL_FILE), front("x", "y")).unwrap();
        std::fs::write(
            src.join(SOURCE_FILE),
            r#"{"url":"https://example.invalid/r","commit":"a1b2c3d"}"#,
        )
        .unwrap();

        install(&t.fs, Scope::Own, &src).unwrap();
        let p = read_provenance(&t.root.join("skills-users/u1/x")).unwrap();
        assert_eq!(p.commit.as_deref(), Some("a1b2c3d"));
        assert!(p.installed_at.is_some(), "install date not stamped");
    }

    #[test]
    fn delete_removes_the_folder_and_refuses_a_path() {
        let t = Tree::new("install-delete", "daniele");
        let src = t.root.join("homes/u1/d");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(SKILL_FILE), front("x", "y")).unwrap();
        install(&t.fs, Scope::Own, &src).unwrap();

        assert!(remove(&t.fs, Scope::Own, "../../etc").is_err());
        assert!(remove(&t.fs, Scope::Own, "nope").is_err());
        remove(&t.fs, Scope::Own, "x").unwrap();
        assert!(crate::skills::list(&t.fs).is_empty());
    }

    /// The card shows the body that will enter the prompt, and on a replacement
    /// it shows the one being replaced — so the human reads a diff, not a name.
    #[test]
    fn the_card_carries_the_whole_skill_body() {
        let t = Tree::new("install-card", "daniele");
        let src = t.root.join("homes/u1/c");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join(SKILL_FILE), front("x", "y")).unwrap();

        let (path, old, new) = preview(&t.fs, Scope::Shared, &src).unwrap();
        assert_eq!(path, "skills/shared/x");
        assert!(old.is_none());
        assert!(new.contains("Body of x."), "{new}");
        assert!(new.contains("every member reads it as instructions"), "{new}");

        install(&t.fs, Scope::Shared, &src).unwrap();
        let (_, old, _) = preview(&t.fs, Scope::Shared, &src).unwrap();
        assert!(old.unwrap().contains("Body of x."));
    }
}
