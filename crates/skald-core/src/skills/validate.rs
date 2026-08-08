//! What a folder must be before it is allowed to become a skill.
//!
//! **One validation site, because there is one door.** The two trees are
//! read-only in both directions (blueprint §9), so every byte that ever lands in
//! them passes through here — and the index on the other side can therefore
//! assume it is reading well-formed skills instead of tolerating half-written
//! ones. That is the whole argument for the read-only trees: with three write
//! paths (fs-tools, the container shell, an HTTP copy) validation would either
//! live in three places or nowhere.
//!
//! This module is deliberately callable from more than the registration tool:
//! the ZIP upload and the marketplace of blueprint §11.2 are the same check with
//! a different source of bytes, and two validators would diverge at the first
//! edge case.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::{SKILL_FILE, parse_front_matter};

/// Longest `name` accepted, matching the `^[a-z0-9][a-z0-9-]{0,63}$` shape: the
/// name becomes a directory name and then a path segment in every prompt.
pub const NAME_MAX: usize = 64;

/// Ceiling on the `description`, applied **here** rather than in the index.
///
/// One limit, at the one place it can fail usefully: the author is present, sees
/// the refusal and can shorten the text. The index's 200-character cut
/// ([`super::DESCRIPTION_LIMIT`]) is a different rule for a different reason —
/// tokens paid on every request of every user — and truncating there is not a
/// rejection, since the full text stays readable through the enumeration tool.
pub const DESCRIPTION_MAX: usize = 1000;

/// Ceiling on how many files one skill may carry.
pub const MAX_FILES: usize = 500;

/// Ceiling on a skill's total size. Generous for instructions plus scripts and
/// reference documents; far below anything that would be a *dataset*, which is
/// not what this tree is for.
pub const MAX_TOTAL_BYTES: u64 = 8 * 1024 * 1024;

/// A source folder that passed every check — the only thing [`super::install`]
/// accepts, so an unvalidated path cannot reach the tree by construction.
#[derive(Debug, Clone)]
pub struct ValidSkill {
    /// The id, taken from the frontmatter `name` and **not** from the source
    /// folder's name. The working copy may be called `draft-2`; the installed
    /// artefact is called what it declares itself to be, and from then on
    /// id = directory name by construction.
    pub id:          String,
    pub description: String,
    /// Every regular file, relative to the source root, in a stable order —
    /// what the approval card lists.
    pub files:       Vec<PathBuf>,
    pub size_bytes:  u64,
}

impl ValidSkill {
    /// Whether the skill carries anything executable, for the enumeration tool.
    pub fn has_scripts(&self) -> bool {
        self.files.iter().any(|f| {
            matches!(
                f.extension().and_then(|e| e.to_str()),
                Some("py" | "js" | "mjs" | "cjs" | "ts" | "sh" | "bash")
            )
        })
    }

    /// Which ecosystem's dependency manifest it ships, if any. Reported rather
    /// than acted on: v1 installs nothing (blueprint §10), and a skill that
    /// needs a package says so in its body.
    pub fn deps(&self) -> Option<String> {
        let has = |n: &str| self.files.iter().any(|f| f == Path::new(n));
        match (has("requirements.txt"), has("package.json")) {
            (true, true)  => Some("python+node".into()),
            (true, false) => Some("python".into()),
            (false, true) => Some("node".into()),
            _             => None,
        }
    }
}

/// Validates a candidate skill folder on the host filesystem.
///
/// Every refusal names what to fix: this error text is read by a model that will
/// try again, and "invalid skill" would only produce a guess.
pub fn validate_dir(dir: &Path) -> Result<ValidSkill> {
    if !dir.is_dir() {
        bail!(
            "not a folder: {}. A skill is a folder containing a `{SKILL_FILE}`.",
            dir.display()
        );
    }

    let skill_md = dir.join(SKILL_FILE);
    if !skill_md.is_file() {
        bail!(
            "no `{SKILL_FILE}` in that folder. A skill is a folder whose `{SKILL_FILE}` \
             opens with a YAML frontmatter block declaring `name` and `description`."
        );
    }

    let body = std::fs::read_to_string(&skill_md)
        .map_err(|e| anyhow::anyhow!("cannot read {SKILL_FILE}: {e}"))?;
    let front = parse_front_matter(&body).map_err(|problem| {
        anyhow::anyhow!(
            "invalid `{SKILL_FILE}` frontmatter ({problem}). It must start with a `---` line, \
             then `name:` and `description:`, then a closing `---`."
        )
    })?;

    check_name(&front.name)?;
    if front.description.chars().count() > DESCRIPTION_MAX {
        bail!(
            "the `description` is {} characters; the limit is {DESCRIPTION_MAX}. It is the \
             *use condition* — when to reach for this skill — not a manual; the body of \
             `{SKILL_FILE}` is where the detail goes.",
            front.description.chars().count()
        );
    }

    let mut walk = Walk::default();
    walk.visit(dir, Path::new(""))?;

    Ok(ValidSkill {
        id:          front.name,
        description: front.description,
        files:       walk.files,
        size_bytes:  walk.bytes,
    })
}

/// The id charset: `^[a-z0-9][a-z0-9-]{0,63}$`.
///
/// Checked by hand rather than by regex because the failure has to *teach* — the
/// caller is a model that will retry, and "does not match a pattern" is not a
/// correction.
fn check_name(name: &str) -> Result<()> {
    if name.len() > NAME_MAX {
        bail!("the frontmatter `name` is longer than {NAME_MAX} characters: `{name}`");
    }
    let ok_first = name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let ok_rest = name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !ok_first || !ok_rest {
        bail!(
            "the frontmatter `name` must be lowercase letters, digits and hyphens, starting \
             with a letter or digit (it becomes the folder name and the path the assistant \
             reads): `{name}`"
        );
    }
    Ok(())
}

/// Recursive walk that enforces the structural rules while it counts.
#[derive(Default)]
struct Walk {
    files: Vec<PathBuf>,
    bytes: u64,
}

impl Walk {
    fn visit(&mut self, dir: &Path, rel: &Path) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", dir.display()))?
            .filter_map(|e| e.ok())
            .collect();
        // Stable order: the file list ends up on an approval card, and a card
        // that reshuffles between two renders of the same folder reads as a
        // different change.
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let path = entry.path();
            let name = entry.file_name();
            let child_rel = rel.join(&name);

            // `symlink_metadata` does NOT follow the link, which is the point:
            // a link is refused for what it is, before anything asks where it
            // points. A skill is copied into a tree every member reads, and a
            // link out of that tree would make the installed artefact a window
            // onto something the installation never reviewed.
            let meta = std::fs::symlink_metadata(&path)
                .map_err(|e| anyhow::anyhow!("cannot stat {}: {e}", child_rel.display()))?;

            if meta.file_type().is_symlink() {
                bail!(
                    "`{}` is a symbolic link. A skill must be self-contained — copy the real \
                     file in instead.",
                    child_rel.display()
                );
            }
            if meta.is_dir() {
                self.visit(&path, &child_rel)?;
                continue;
            }
            if !meta.is_file() {
                bail!("`{}` is not a regular file.", child_rel.display());
            }

            self.bytes += meta.len();
            self.files.push(child_rel);
            if self.files.len() > MAX_FILES {
                bail!("that folder holds more than {MAX_FILES} files — too much for a skill.");
            }
            if self.bytes > MAX_TOTAL_BYTES {
                bail!(
                    "that folder is over {} MiB — too much for a skill. Skills hold \
                     instructions, scripts and reference documents; bulk data belongs in your \
                     home or a project.",
                    MAX_TOTAL_BYTES / (1024 * 1024)
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "skald-skillval-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        fn write(&self, rel: &str, body: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn front(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\nBody.\n")
    }

    /// The id comes from the frontmatter, never from the folder: the working
    /// copy is allowed a scratch name, the artefact is not.
    #[test]
    fn the_id_comes_from_the_frontmatter_not_the_folder() {
        let d = Dir::new("id");
        d.write("SKILL.md", &front("ics-import", "Import an ICS feed."));
        d.write("scripts/run.py", "print(1)\n");
        let v = validate_dir(&d.0).unwrap();
        assert_eq!(v.id, "ics-import");
        assert_eq!(v.files, vec![PathBuf::from("SKILL.md"), PathBuf::from("scripts/run.py")]);
        assert!(v.has_scripts());
        assert_eq!(v.deps(), None);
    }

    #[test]
    fn a_folder_without_a_skill_md_is_refused() {
        let d = Dir::new("nomd");
        d.write("notes.txt", "hello");
        let e = validate_dir(&d.0).unwrap_err().to_string();
        assert!(e.contains("SKILL.md"), "{e}");
    }

    #[test]
    fn a_name_that_cannot_be_a_folder_is_refused() {
        for bad in ["Ics Import", "../escape", "-leading", "UPPER"] {
            let d = Dir::new("badname");
            d.write("SKILL.md", &front(bad, "x"));
            assert!(validate_dir(&d.0).is_err(), "accepted `{bad}`");
        }
    }

    #[test]
    fn an_overlong_description_is_refused_here_not_truncated() {
        let d = Dir::new("longdesc");
        d.write("SKILL.md", &front("x", &"d".repeat(DESCRIPTION_MAX + 1)));
        let e = validate_dir(&d.0).unwrap_err().to_string();
        assert!(e.contains(&DESCRIPTION_MAX.to_string()), "{e}");
    }

    /// A symlink is refused for being one, without asking where it points: the
    /// installed copy is read as instruction by everyone the scope covers.
    #[cfg(unix)]
    #[test]
    fn a_symlink_anywhere_inside_is_refused() {
        let d = Dir::new("symlink");
        d.write("SKILL.md", &front("x", "y"));
        std::os::unix::fs::symlink("/etc/passwd", d.0.join("secrets.txt")).unwrap();
        let e = validate_dir(&d.0).unwrap_err().to_string();
        assert!(e.contains("symbolic link"), "{e}");
    }

    #[test]
    fn dependency_manifests_are_reported_not_installed() {
        let d = Dir::new("deps");
        d.write("SKILL.md", &front("x", "y"));
        d.write("requirements.txt", "requests\n");
        assert_eq!(validate_dir(&d.0).unwrap().deps().as_deref(), Some("python"));
    }
}
