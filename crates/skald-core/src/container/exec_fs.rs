//! Filesystem primitives that act **inside** a user's container, for the paths
//! their bind mounts do not cover (`/tmp`, `/etc`, an installed package's files…).
//!
//! The security boundary is the container, not the bind-mounted subtree: an agent
//! already has unrestricted reach in there through `execute_cmd`, which runs with
//! passwordless `sudo`. Tools that stopped at the mounts were therefore not
//! protecting anything — they offered a poorer view of the same sandbox, and the
//! model routinely worked around them by shelling out. These primitives close
//! that gap so the fs-tools see what the shell sees.
//!
//! What does *not* change is host containment. A path that lands on a mount keeps
//! the host fast path and its canonicalize-and-prefix-check, which is what stops a
//! symlink planted in the container from resolving against the **host's** `/etc`.
//! Nothing here ever touches the host filesystem, so there is no host to escape
//! from on this side.
//!
//! Paths are passed to `sh` **positionally** (`$1`), never interpolated into the
//! script, so a path containing quotes or `$(…)` is data and not shell syntax —
//! the same rule `execute_cmd` already follows for its pidfile.

use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;

/// Runs a shell snippet inside `container` with `args` bound to `$1`, `$2`, …
/// Returns raw stdout — callers that expect text decode it themselves, so a
/// binary `cat` is not mangled on the way through.
pub(super) async fn sh(container: &str, script: &str, args: &[&str]) -> Result<Vec<u8>> {
    let mut argv: Vec<&str> = vec!["exec", container, "sh", "-c", script, "_"];
    argv.extend_from_slice(args);

    let out = tokio::process::Command::new("docker")
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to spawn `docker` (is the Docker CLI installed?)")?;

    if out.status.success() {
        Ok(out.stdout)
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{}", err.trim());
    }
}

/// True when the snippet exits 0 — for the `test`-style probes, where a non-zero
/// exit is the answer rather than a failure.
async fn sh_ok(container: &str, script: &str, args: &[&str]) -> bool {
    sh(container, script, args).await.is_ok()
}

/// Reads a file from inside the container.
pub async fn read(container: &str, path: &Path) -> Result<Vec<u8>> {
    let p = path.to_string_lossy();
    sh(container, r#"cat -- "$1""#, &[&p])
        .await
        .with_context(|| format!("Cannot read file: {p}"))
}

/// Writes a file inside the container, creating its parent directories. The
/// bytes travel on stdin rather than inside the script, so content is never
/// shell-parsed and size is bounded by the pipe, not by `ARG_MAX`.
pub async fn write(container: &str, path: &Path, bytes: &[u8]) -> Result<()> {
    let p = path.to_string_lossy();
    let mut child = tokio::process::Command::new("docker")
        .args([
            "exec", "-i", container, "sh", "-c",
            r#"mkdir -p -- "$(dirname -- "$1")" && cat > "$1""#, "_", &p,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn `docker`")?;

    child
        .stdin
        .take()
        .context("docker exec produced no stdin")?
        .write_all(bytes)
        .await
        .with_context(|| format!("Failed to write: {p}"))?;

    let out = child.wait_with_output().await.context("docker exec failed")?;
    if !out.status.success() {
        bail!("Failed to write {p}: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub async fn exists(container: &str, path: &Path) -> bool {
    sh_ok(container, r#"test -e "$1""#, &[&path.to_string_lossy()]).await
}

pub async fn is_dir(container: &str, path: &Path) -> bool {
    sh_ok(container, r#"test -d "$1""#, &[&path.to_string_lossy()]).await
}

/// One entry of a container directory listing.
pub struct Entry {
    pub name:   String,
    pub is_dir: bool,
    pub size:   u64,
}

/// Lists a directory inside the container, `depth` levels deep (1 = immediate
/// children). Emits `type\tsize\tpath` per line via `find`, which is in the image
/// and needs no parsing of `ls`'s locale-dependent output.
pub async fn list(container: &str, path: &Path, depth: usize) -> Result<Vec<Entry>> {
    let p = path.to_string_lossy();
    let d = depth.max(1).to_string();
    let raw = sh(
        container,
        r#"find "$1" -mindepth 1 -maxdepth "$2" -printf '%y\t%s\t%p\n' 2>/dev/null || true"#,
        &[&p, &d],
    )
    .await
    .with_context(|| format!("Cannot list directory: {p}"))?;

    let text = String::from_utf8_lossy(&raw);
    let prefix = format!("{}/", p.trim_end_matches('/'));
    let mut out = Vec::new();
    for line in text.lines() {
        let mut f = line.splitn(3, '\t');
        let (Some(kind), Some(size), Some(full)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        out.push(Entry {
            name:   full.strip_prefix(&prefix).unwrap_or(full).to_string(),
            is_dir: kind == "d",
            size:   size.parse().unwrap_or(0),
        });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(out)
}
