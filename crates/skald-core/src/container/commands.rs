//! What the agent is told its sandbox can run — a **discovery aid, not an
//! inventory**.
//!
//! The failure this closes is upstream of any tool call: an agent that does not
//! know `ffmpeg` is installed either declines the job or spends a round finding
//! out. So the point is to make the common case answerable without a round-trip,
//! and nothing more. It follows that:
//!
//! - **The list is curated, not discovered.** `ls /usr/bin` is 800 entries of
//!   coreutils noise; a hint that long is not a hint. [`PROBE_ALLOWLIST`] is the
//!   curation — the image's own toolbelt plus the handful of things an agent
//!   plausibly installs — and its **order is meaningful** (grouped by the kind of
//!   work), which is why nothing here sorts.
//! - **The probe exists so the list cannot lie**, not so it can discover. A
//!   hand-maintained list drifts from the image, and a container recreate throws
//!   away everything an agent installed with apt; `command -v` at login means we
//!   never announce something that is not there.
//! - **Incompleteness is stated, not hidden.** The rendered section says the list
//!   is partial and that more can be installed — so a tool outside the allowlist
//!   costs the agent one `command -v`, which is what it would have paid anyway.
//!
//! Because it is a hint, staleness is cheap in both directions: a mid-session
//! install is known to the agent that performed it, and a container recreate
//! costs one `not found` plus an `apt-get install` on a path the agent was
//! already walking. Hence a plain login-time snapshot, refreshed at the next
//! login, and no invalidation machinery.

use std::time::Duration;

use anyhow::{Context, Result};

/// How long the probe may take before login gives up on it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The commands worth spending prompt tokens on, in the order they are rendered.
///
/// Grouped by the kind of work, because the reader is a model deciding whether
/// it can do a job — related tools next to each other is the whole value of a
/// curated list over a sorted one. Two kinds of entry live here: what
/// `container/Dockerfile` installs, and what an agent plausibly adds with
/// `sudo apt-get install` (`pandoc`, `cargo`, `yt-dlp`…) — the latter appear
/// only once actually installed, at the next login.
///
/// Keep it short. Every addition is paid on every request of every agent that
/// can run commands, and a list long enough to skim is a list that stopped
/// being a hint.
pub const PROBE_ALLOWLIST: &[&str] = &[
    // Runtimes and package managers.
    "python3", "pip3", "node", "npm", "cargo", "go", "php", "perl",
    // Media.
    "ffmpeg", "ffprobe", "convert", "yt-dlp",
    // Documents and OCR.
    "pdftotext", "pdftoppm", "tesseract", "pandoc",
    // Text, data, search.
    "jq", "rg", "sqlite3", "file",
    // Archives.
    "unzip", "zip", "tar", "xz", "gzip",
    // Network and source control.
    "curl", "wget", "git", "ssh", "rsync", "dig",
    // Build.
    "make", "gcc", "g++",
];

/// The shell snippet run inside the container: one `command -v` per allowlist
/// entry, printing the ones that resolve.
///
/// `exit 0` is load-bearing — without it the script's status is that of the last
/// `command -v`, so a container missing the final entry would look like a failed
/// probe. Entries are interpolated rather than passed positionally because they
/// are compile-time constants restricted to `[a-z0-9+._-]` (asserted by
/// `allowlist_is_shell_safe`), unlike the user-supplied paths in `exec_fs`.
pub fn probe_script() -> String {
    let mut s = String::from("for c in");
    for c in PROBE_ALLOWLIST {
        s.push(' ');
        s.push_str(c);
    }
    s.push_str("; do command -v \"$c\" >/dev/null 2>&1 && echo \"$c\"; done; exit 0");
    s
}

/// Parses the probe's stdout: one command per line, blanks dropped, duplicates
/// collapsed, **order preserved** (the script walks the allowlist, so its output
/// already carries the curation).
pub fn parse_probe_output(stdout: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in stdout.lines() {
        let name = line.trim();
        if name.is_empty() || out.iter().any(|c| c == name) {
            continue;
        }
        out.push(name.to_string());
    }
    out
}

/// Probes `container` for the allowlisted commands it actually has.
///
/// One `docker exec`, bounded by [`PROBE_TIMEOUT`]. Callers treat a failure as an
/// empty list: this is a hint, and login must never fail for it.
pub async fn probe_container_commands(container: &str) -> Result<Vec<String>> {
    let stdout = tokio::time::timeout(
        PROBE_TIMEOUT,
        super::exec_fs::sh(container, &probe_script(), &[]),
    )
    .await
    .map_err(|_| anyhow::anyhow!("sandbox command probe timed out after {PROBE_TIMEOUT:?}"))?
    .context("sandbox command probe failed")?;

    Ok(parse_probe_output(&String::from_utf8_lossy(&stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allowlist is interpolated straight into a shell script, so every entry
    /// must be inert there. This is the check that lets `probe_script` skip the
    /// positional-argument dance `exec_fs` needs for user-supplied paths.
    #[test]
    fn allowlist_is_shell_safe() {
        for c in PROBE_ALLOWLIST {
            assert!(
                !c.is_empty()
                    && c.chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || "+._-".contains(ch)),
                "allowlist entry is not shell-safe: {c:?}"
            );
        }
    }

    #[test]
    fn allowlist_has_no_duplicates() {
        let mut seen: Vec<&str> = Vec::new();
        for c in PROBE_ALLOWLIST {
            assert!(!seen.contains(c), "duplicate allowlist entry: {c}");
            seen.push(c);
        }
    }

    /// A container missing the *last* allowlist entry must not read as a failed
    /// probe — see the `exit 0` note on `probe_script`.
    #[test]
    fn probe_script_always_exits_zero() {
        assert!(probe_script().ends_with("exit 0"));
    }

    #[test]
    fn parse_drops_blanks_and_duplicates_and_keeps_order() {
        let out = parse_probe_output("ffmpeg\n\n  jq  \nffmpeg\ngit\n");
        assert_eq!(out, vec!["ffmpeg", "jq", "git"]);
    }

    #[test]
    fn parse_of_nothing_is_empty() {
        assert!(parse_probe_output("").is_empty());
        assert!(parse_probe_output("\n  \n").is_empty());
    }
}
