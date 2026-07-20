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
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::container::{CONTAINER_HOME, HOMES_DIR};

/// Subdirectory of the working directory holding installed connector folders.
pub const CONNECTORS_DIR: &str = "connectors";

/// The manifest, saved verbatim at install time as provenance (never read back).
pub const MANIFEST_FILE: &str = "connector.json";

/// Marker file in a user's installed connector dir recording the content hash of
/// the source folder that produced the current files + dependencies. When the
/// source changes (a marketplace update) this hash changes, and the reconciler
/// re-copies + re-installs; when it matches, a startup is a cheap no-op.
const INSTALL_LOCK: &str = ".skald-install.lock";

/// Where `pip install --target` lands a python connector's dependencies, a sibling
/// of the server file inside the connector dir. Injected onto the server process's
/// `PYTHONPATH` at spec-build time (see `mcp::user_row_spec`). Node needs no
/// equivalent: `node_modules/` beside the entry file is resolved automatically.
pub const PYDEPS_DIR: &str = ".pydeps";

/// Ceiling for a single `npm`/`pip` install inside the container. Baileys or a
/// heavy python wheel set can take a while on a cold cache; a genuinely stuck
/// install must still fail rather than hang a login forever.
const DEPS_INSTALL_TIMEOUT_SECS: u64 = 300;

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

// ── dependency reconciler (blueprint §6/§7) ─────────────────────────────────────
//
// Copying a connector's files into a container never made its dependencies exist
// there: a python server still needs its wheels, a node server its `node_modules`.
// [`ensure_installed`] closes that gap and keeps it closed across updates.
//
// It is a **content-hash reconciler**, not a one-shot installer. The trigger is a
// hash of the *source* folder's runtime files, not a version string the author
// might forget to bump: any real change to what the connector ships (including its
// `package.json` / `requirements.txt`) changes the hash and forces a refresh. It
// runs on every per-user startup path (first login, container recreate/remount)
// and at activation, so:
//   - a brand-new container (no home files) installs from scratch,
//   - an updated connector (source changed) re-copies + re-installs,
//   - an unchanged one (hash matches the lock) is skipped in microseconds.

/// Reconciles user `user_id`'s copy of local-script connector `folder` (runtime
/// name `runtime_name`) inside `container`: refreshes the files when the source
/// changed, then (re)installs node and/or python dependencies. Idempotent and
/// hash-guarded. Dependency install is best-effort at the call site (a failure is
/// returned, and callers log-and-continue so the server still starts and surfaces
/// its own import error) — but a changed source with a failed install does NOT
/// write the lock, so the next startup retries.
pub async fn ensure_installed(
    user_id:      &str,
    runtime_name: &str,
    folder:       &str,
    container:    &str,
) -> Result<()> {
    let src = connector_dir(folder)?;
    if !src.is_dir() {
        // Nothing shipped for this connector on this box; leave any existing files
        // in place (a caller that truly needs them says so with its own message).
        return Ok(());
    }
    let home = home_dir_for(user_id, runtime_name)?;
    let lock = home.join(INSTALL_LOCK);
    let src_hash = hash_source(&src)?;
    if let Ok(prev) = std::fs::read_to_string(&lock) {
        if prev.trim() == src_hash {
            return Ok(()); // files + deps already current
        }
    }

    // (Re)copy source files. `install_into_home` overwrites shipped files but never
    // deletes others, so the durable `auth/`, `node_modules/`, `.pydeps/` and the
    // lock itself survive an update.
    let container_dir = match install_into_home(user_id, runtime_name, folder)? {
        Some(d) => d,
        None => return Ok(()),
    };

    // Install whatever ecosystem the connector ships. A connector may ship both.
    if home.join("package.json").is_file() {
        run_in_container(
            container,
            &container_dir,
            // `npm ci` is reproducible when a lockfile is present; fall back to
            // `npm install` when it is not (or when ci rejects a drifted lock).
            "npm ci --omit=dev --no-audit --no-fund 2>&1 || npm install --omit=dev --no-audit --no-fund 2>&1",
            "npm",
        )
        .await?;
    }
    if home.join("requirements.txt").is_file() {
        run_in_container(
            container,
            &container_dir,
            // `--target .pydeps` keeps deps beside the server (durable, per-connector)
            // and out of the PEP-668 externally-managed system site; `--break-system-
            // packages` silences that guard even though `--target` already avoids it.
            &format!(
                "python3 -m pip install --break-system-packages --target {PYDEPS_DIR} \
                 -r requirements.txt 2>&1"
            ),
            "pip",
        )
        .await?;
    }

    std::fs::write(&lock, &src_hash)
        .with_context(|| format!("failed to write {}", lock.display()))?;
    Ok(())
}

/// A deterministic content hash of a connector's **runtime** files (host assets —
/// icons, `connector.json` — excluded, since they never reach the container and so
/// cannot change what runs). Path + bytes of every file, sorted, folded into one
/// SHA-256. Two installs of the same source produce the same hash on any box.
fn hash_source(src: &Path) -> Result<String> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    collect_source_files(src, Path::new(""), &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = Sha256::new();
    for (rel, bytes) in files {
        h.update(rel.as_bytes());
        h.update([0u8]);
        h.update(&bytes);
        h.update([0u8]);
    }
    Ok(h.finalize().iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    }))
}

fn collect_source_files(dir: &Path, rel: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let entry = entry?;
        let child_rel = rel.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            collect_source_files(&entry.path(), &child_rel, out)?;
            continue;
        }
        let rel_str = child_rel.to_string_lossy().to_string();
        if is_host_asset(&rel_str) {
            continue;
        }
        let bytes = std::fs::read(entry.path())
            .with_context(|| format!("cannot read {}", entry.path().display()))?;
        out.push((rel_str, bytes));
    }
    Ok(())
}

/// Runs a shell `script` inside `container` at `workdir` via `docker exec`, under a
/// timeout, and fails with the tail of the output on a non-zero exit. Output is not
/// captured into the DB — only surfaced in the returned error for the caller's log.
async fn run_in_container(container: &str, workdir: &Path, script: &str, label: &str) -> Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(DEPS_INSTALL_TIMEOUT_SECS),
        tokio::process::Command::new("docker")
            .arg("exec")
            .arg("-w").arg(workdir)
            .arg(container)
            .arg("sh").arg("-c").arg(script)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{label} install timed out after {DEPS_INSTALL_TIMEOUT_SECS}s"))?
    .with_context(|| format!("failed to run docker exec for {label} install"))?;

    if !output.status.success() {
        let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        let tail: String = combined.lines().rev().take(12).collect::<Vec<_>>()
            .into_iter().rev().collect::<Vec<_>>().join("\n");
        bail!("{label} install failed:\n{tail}");
    }
    Ok(())
}

/// Installs a **global** connector's dependencies on the HOST, into `.pydeps`
/// (python) / `node_modules` (node) beside its files in `connectors/<folder>/`.
///
/// The host counterpart of [`ensure_installed`]: a `global` connector runs in the
/// Skald process, not a container (§7), so its declared deps must resolve on the
/// host — `global_row_spec` puts `<dir>/.pydeps` on the server's `PYTHONPATH`. Unlike
/// the per-user reconciler this is not hash-guarded: the deps land in the same
/// `connectors/<folder>/` tree the hash would cover, so it simply relies on `pip`/
/// `npm` being idempotent (a satisfied requirement is a fast no-op). Called at
/// enable time; the installed `.pydeps` is durable and survives a restart, so the
/// boot relaunch needs no reinstall.
pub async fn ensure_installed_host(folder: &str) -> Result<()> {
    let dir = connector_dir(folder)?;
    if !dir.is_dir() {
        // Nothing shipped for this connector on this box; a caller that truly needs
        // the files fails later with its own message.
        return Ok(());
    }
    if dir.join("package.json").is_file() {
        run_on_host(
            &dir,
            "npm ci --omit=dev --no-audit --no-fund 2>&1 || npm install --omit=dev --no-audit --no-fund 2>&1",
            "npm",
        )
        .await?;
    }
    if dir.join("requirements.txt").is_file() {
        run_on_host(
            &dir,
            &format!(
                "python3 -m pip install --break-system-packages --target {PYDEPS_DIR} \
                 -r requirements.txt 2>&1"
            ),
            "pip",
        )
        .await?;
    }
    Ok(())
}

/// Runs a shell `script` on the HOST at `workdir`, under the same install timeout,
/// failing with the tail of the output on a non-zero exit. The host counterpart of
/// [`run_in_container`], for a `global` connector whose deps live beside its files in
/// `connectors/<id>/` rather than inside a container.
async fn run_on_host(workdir: &Path, script: &str, label: &str) -> Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(DEPS_INSTALL_TIMEOUT_SECS),
        tokio::process::Command::new("sh")
            .arg("-c").arg(script)
            .current_dir(workdir)
            .output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("{label} install timed out after {DEPS_INSTALL_TIMEOUT_SECS}s"))?
    .with_context(|| format!("failed to run {label} install on host"))?;

    if !output.status.success() {
        let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        let tail: String = combined.lines().rev().take(12).collect::<Vec<_>>()
            .into_iter().rev().collect::<Vec<_>>().join("\n");
        bail!("{label} install failed:\n{tail}");
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
