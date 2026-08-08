//! The administrative view of the two trees — what `list_items(type="skills")`
//! returns (blueprint §7.7).
//!
//! **Not the index, and the distinction is worth keeping sharp.** The index is
//! for *deciding*: path plus a cut description, the minimum needed to tell
//! whether a skill bears on the request, injected into every prompt and paid for
//! in tokens on every request of every user. This is for *administering*: the
//! full description, size, health and provenance, as JSON, only when asked. One
//! is always there and thin; the other is on demand and complete.
//!
//! Two consequences fall out of that split:
//!
//! - The **description is not truncated here.** Truncate in both places and the
//!   full text becomes unreadable anywhere, while this tool exists precisely to
//!   be the place it can be read. The real ceiling belongs at registration
//!   ([`super::validate::DESCRIPTION_MAX`]), where the author is present and the
//!   refusal is useful.
//! - **A broken skill appears here**, with the reason. The index skips it —
//!   correctly, since it cannot be trusted to describe itself — but then nothing
//!   would say *why* a folder placed on the box never showed up.

use std::path::Path;

use core_api::user_fs::{SKILLS_ROOT, SKILLS_SHARED_SCOPE, UserFs};
use serde_json::{Value, json};

use super::install::read_provenance;
use super::validate::validate_dir;
use super::{DESCRIPTION_LIMIT, SKILL_FILE, parse_front_matter};

/// Every skill folder visible to this user — valid or not — as JSON rows, in the
/// index's order (group's tree first, then their own, each by id).
pub fn report(fs: &UserFs) -> Vec<Value> {
    let Some(sk) = &fs.skills else { return Vec::new() };

    let mut rows: Vec<(String, String, Value)> = Vec::new();
    for (scope, host) in [
        (SKILLS_SHARED_SCOPE, sk.shared_host.as_path()),
        (sk.own_username.as_str(), sk.own_host.as_path()),
    ] {
        let Ok(entries) = std::fs::read_dir(host) else { continue };
        let mut ids: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            // Dot-directories are plumbing (a staging leftover), never a skill.
            .filter(|id| !id.starts_with('.'))
            .collect();
        ids.sort();
        for id in ids {
            let row = inspect(scope, &host.join(&id), &id);
            rows.push((id, scope.to_string(), row));
        }
    }

    // A collision is a property of the *pair*, so it can only be marked once
    // both trees have been read — the same reason the index marks both lines.
    let colliding: std::collections::HashSet<String> = rows
        .iter()
        .filter(|(id, scope, _)| rows.iter().any(|(o, os, _)| o == id && os != scope))
        .map(|(id, _, _)| id.clone())
        .collect();

    rows.into_iter()
        .map(|(id, _, mut row)| {
            row["collision"] = Value::Bool(colliding.contains(&id));
            row
        })
        .collect()
}

/// One folder, described as fully as it allows itself to be.
fn inspect(scope: &str, dir: &Path, id: &str) -> Value {
    let path = format!("{SKILLS_ROOT}/{scope}/{id}");
    let mut row = json!({
        "id":       id,
        "scope":    scope,
        "path":     path,
        "valid":    false,
        "problem":  Value::Null,
    });

    // Validity here means **what the index does**: does its frontmatter parse?
    // The structural rules (`validate_dir`) are a stricter set — they gate what
    // may be *installed* — so a folder that fails them but parses is still shown
    // in the prompt and must be reported as such, with the extra problem named.
    let body = match std::fs::read_to_string(dir.join(SKILL_FILE)) {
        Ok(b) => b,
        Err(e) => {
            row["problem"] = json!(format!("no readable {SKILL_FILE}: {e}"));
            return row;
        }
    };
    let front = match parse_front_matter(&body) {
        Ok(f) => f,
        Err(problem) => {
            row["problem"] = json!(format!("invalid frontmatter: {problem}"));
            return row;
        }
    };

    row["valid"] = Value::Bool(true);
    row["description"] = json!(front.description);
    row["truncated_in_index"] =
        Value::Bool(front.description.chars().count() > DESCRIPTION_LIMIT);
    if front.name != id {
        row["problem"] = json!(format!(
            "the frontmatter `name` is `{}` but the folder is `{id}`; the folder wins",
            front.name
        ));
    }

    match validate_dir(dir) {
        Ok(v) => {
            row["files"] = json!(v.files.len());
            row["size_bytes"] = json!(v.size_bytes);
            row["has_scripts"] = Value::Bool(v.has_scripts());
            row["deps"] = v.deps().map(Value::String).unwrap_or(Value::Null);
        }
        // Reachable only for a folder placed by hand: everything installed
        // through `skill_register` passed this very check.
        Err(e) => row["problem"] = json!(e.to_string()),
    }

    if let Some(p) = read_provenance(dir) {
        row["source_url"] = json!(p.url);
        row["commit"] = p.commit.map(Value::String).unwrap_or(Value::Null);
        row["installed_at"] = p.installed_at.map(Value::String).unwrap_or(Value::Null);
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::tests_support::{Tree, valid};

    /// The full description survives here even when the index cut it — this is
    /// the one place it can be read whole.
    #[test]
    fn the_description_is_reported_untruncated_and_flagged() {
        let long = "d".repeat(DESCRIPTION_LIMIT + 40);
        let t = Tree::new("inv-desc", "daniele");
        t.write("shared", "x", &valid("x", &long));

        let rows = report(&t.fs);
        assert_eq!(rows[0]["description"], json!(long));
        assert_eq!(rows[0]["truncated_in_index"], json!(true));
        assert_eq!(rows[0]["path"], json!("skills/shared/x"));
    }

    /// A folder the index skipped still shows up, with the reason — otherwise
    /// nothing anywhere answers "why did my skill never appear?".
    #[test]
    fn a_broken_skill_is_reported_with_its_problem() {
        let t = Tree::new("inv-broken", "daniele");
        t.write("shared", "good", &valid("good", "Works."));
        t.write("shared", "broken", "no frontmatter at all\n");

        let rows = report(&t.fs);
        assert_eq!(rows.len(), 2);
        let broken = rows.iter().find(|r| r["id"] == json!("broken")).unwrap();
        assert_eq!(broken["valid"], json!(false));
        assert!(broken["problem"].as_str().unwrap().contains("frontmatter"));
        // …and the index really did skip it, so the two views agree on the facts
        // while disagreeing on what they show.
        assert_eq!(crate::skills::list(&t.fs).len(), 1);
    }

    #[test]
    fn a_colliding_id_is_marked_on_both_rows() {
        let t = Tree::new("inv-collide", "daniele");
        t.write("shared", "x", &valid("x", "Group's."));
        t.write("mine", "x", &valid("x", "Mine."));
        t.write("mine", "y", &valid("y", "Untouched."));

        let rows = report(&t.fs);
        for r in &rows {
            let expect = r["id"] == json!("x");
            assert_eq!(r["collision"], json!(expect), "{r}");
        }
    }
}
