//! On-disk layout of installed connectors (blueprint §7/§14).
//!
//! One folder per connector, `{WD}/connectors/<name>/`, holding exactly what the
//! marketplace served: the runtime files, the icons, and the `connector.json` the
//! admin accepted. It sits beside `homes/` and `shared/` because it belongs to the
//! **instance**, not to the checkout — `scripts/` was the wrong home for it, being
//! a source-tree directory that also carries hand-written dev scripts.
//!
//! Two consumers, and the split matters:
//!
//! - A **global** connector runs on the host, straight out of this folder.
//! - A **per-user** connector runs inside the user's container, so its runtime
//!   files are copied into the bind-mounted home ([`install_into_home`]) — the only
//!   durable zone (§6), so they survive a container recreate.
//!
//! `connector.json` is written but never read back: [`crate::db::mcp_catalog`] is
//! the only thing that drives a connect. The file is provenance — what was accepted,
//! and on what day — which is also what makes a later silent upstream change
//! detectable. Reading it at runtime would create a second source of truth that
//! diverges the moment the admin edits the catalog row.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::container::{CONTAINER_HOME, HOMES_DIR};

/// Subdirectory of the working directory holding installed connector folders.
pub const CONNECTORS_DIR: &str = "connectors";

/// The manifest, saved verbatim at install time as provenance (never read back).
pub const MANIFEST_FILE: &str = "connector.json";

/// Where a per-user connector's files land inside the container, under the home
/// mount. `{CONTAINER_HOME}/.skald/mcp/<runtime_name>/`.
const IN_CONTAINER_MCP_SUBDIR: &str = ".skald/mcp";

/// The host directory holding `name`'s installed files. Does not check existence —
/// callers that need the files present say so themselves, with their own message.
pub fn connector_dir(name: &str) -> Result<PathBuf> {
    let wd = std::env::current_dir().context("failed to read working directory")?;
    Ok(wd.join(CONNECTORS_DIR).join(name))
}

/// Splits a catalog `script_path` (`<folder>/<rel>`) into the connector folder and
/// the entry file's path *inside* it.
///
/// The tail is kept whole rather than reduced to a basename: a connector may ship a
/// tree (`pkg/server.py`), and flattening it would break the import that made it a
/// tree in the first place.
pub fn split_script_path(script_path: &str) -> Result<(&str, &str)> {
    match script_path.split_once('/') {
        Some((folder, rel)) if !folder.is_empty() && !rel.is_empty() => Ok((folder, rel)),
        _ => bail!("script_path `{script_path}` is not of the form `<connector>/<file>`"),
    }
}

/// Whether a file is a host-side asset rather than something the runtime needs.
///
/// Icons are for the browser and the manifest is provenance; neither has any job
/// inside a user's container, so they stay out of the home. The rule is extension-
/// based because the manifest names icons freely (`icon_sm.png`, `icon_lg.svg`);
/// if some future connector ever ships an image it genuinely needs at runtime, this
/// is the one place to reconsider.
pub fn is_host_asset(rel: &str) -> bool {
    if rel == MANIFEST_FILE {
        return true;
    }
    let ext = Path::new(rel)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(ext.as_str(), "png" | "svg" | "jpg" | "jpeg" | "webp" | "gif" | "ico")
}

/// The in-container path of a per-user connector's directory.
fn container_dir_for(runtime_name: &str) -> PathBuf {
    Path::new(CONTAINER_HOME).join(IN_CONTAINER_MCP_SUBDIR).join(runtime_name)
}

/// The host path of a per-user connector's directory, inside the bind-mounted home.
fn home_dir_for(user_id: &str, runtime_name: &str) -> Result<PathBuf> {
    let wd = std::env::current_dir().context("failed to read working directory")?;
    Ok(wd
        .join(HOMES_DIR)
        .join(user_id)
        .join(IN_CONTAINER_MCP_SUBDIR)
        .join(runtime_name))
}

/// Copies the runtime files of the installed connector `folder` into `user_id`'s
/// home under `.skald/mcp/<runtime_name>/`, and returns the directory's path
/// **inside** the container.
///
/// The whole tree is copied, minus host assets ([`is_host_asset`]) — which is what
/// finally gets a connector's `requirements.txt` and its multi-file trees into the
/// container, where copying a single entry file never did.
///
/// Returns `Ok(None)` when `folder` was never installed on this box, so a caller
/// that does not actually need the files (a catalog entry pointing at nothing, a
/// connector with no verify step) can carry on. Idempotent: re-running overwrites.
pub fn install_into_home(
    user_id: &str,
    runtime_name: &str,
    folder: &str,
) -> Result<Option<PathBuf>> {
    let src = connector_dir(folder)?;
    if !src.is_dir() {
        return Ok(None);
    }
    let dest = home_dir_for(user_id, runtime_name)?;
    std::fs::create_dir_all(&dest)
        .with_context(|| format!("failed to create {}", dest.display()))?;
    copy_runtime_files(&src, &dest, Path::new(""))?;
    Ok(Some(container_dir_for(runtime_name)))
}

/// Recursively copies `src` into `dest`, skipping host assets. `rel` tracks the
/// path relative to the connector root so [`is_host_asset`] sees the same string
/// the manifest declared.
fn copy_runtime_files(src: &Path, dest: &Path, rel: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src).with_context(|| format!("cannot read {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let child_rel = rel.join(&name);
        let from = entry.path();
        let to = dest.join(&name);

        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&to)
                .with_context(|| format!("failed to create {}", to.display()))?;
            copy_runtime_files(&from, &to, &child_rel)?;
            continue;
        }
        if is_host_asset(&child_rel.to_string_lossy()) {
            continue;
        }
        std::fs::copy(&from, &to)
            .with_context(|| format!("failed to copy {}", child_rel.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_a_script_path_into_folder_and_tail() {
        assert_eq!(split_script_path("gmail/server.py").unwrap(), ("gmail", "server.py"));
        // A tree keeps its shape — the tail is not reduced to a basename.
        assert_eq!(
            split_script_path("whatsapp/pkg/index.js").unwrap(),
            ("whatsapp", "pkg/index.js")
        );
        for bad in ["server.py", "", "gmail/", "/server.py"] {
            assert!(split_script_path(bad).is_err(), "should have rejected `{bad}`");
        }
    }

    /// Icons and the manifest are host-side only: they must never reach a user's
    /// container, while everything the server actually runs on must.
    #[test]
    fn host_assets_are_icons_and_the_manifest() {
        for asset in ["connector.json", "icon_sm.png", "icon_lg.svg", "a/b/logo.WEBP"] {
            assert!(is_host_asset(asset), "`{asset}` should be a host asset");
        }
        for runtime in ["server.py", "requirements.txt", "pkg/index.js", "verify.py"] {
            assert!(!is_host_asset(runtime), "`{runtime}` should reach the container");
        }
    }

    #[test]
    fn container_dir_hangs_off_the_home_mount() {
        assert_eq!(
            container_dir_for("gmail"),
            PathBuf::from("/root/.skald/mcp/gmail")
        );
    }
}
