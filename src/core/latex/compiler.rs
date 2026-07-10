//! `LatexCompiler` — compiles `.tex` sources to PDF using `latexmk -xelatex`.
//!
//! ## Caching (dependency-aware)
//! LaTeX documents routinely pull in external fragments via `\input`,
//! `\include`, `\includegraphics`, custom `.sty`/`.cls` packages, `.bib`
//! files, and so on. A cache keyed only on the main `.tex` content would serve
//! stale PDFs whenever one of those dependencies changes, so we use the
//! `.fls` recorder file produced by TeX (and orchestrated by `latexmk`) to
//! discover the full set of inputs and key the cache on their combined
//! content hash.
//!
//! Two cache artefacts live under `<tmp>/skald-latex/`:
//!
//! | Artefact              | Key                                 | Purpose                                  |
//! |-----------------------|-------------------------------------|------------------------------------------|
//! | `<path-hash>.fls`     | SHA-256 of the `.tex` absolute path | Last-known input list for that source    |
//! | `<deps-hash>.pdf`     | SHA-256 of every input's contents   | The compiled PDF for that exact state    |
//!
//! Lookup flow per request:
//! 1. Read `<path-hash>.fls`. If missing → fresh compile.
//! 2. Parse it, keep only user-controlled inputs (see [`parse_user_deps`]),
//!    hash every file's bytes, derive `<deps-hash>`.
//! 3. If `<deps-hash>.pdf` exists → cache hit, serve it.
//! 4. Otherwise → run `latexmk`, capture the new `.fls`, overwrite the
//!    `<path-hash>.fls` sidecar, save the PDF as `<deps-hash>.pdf`, serve.
//!
//! `latexmk` runs in a per-compile scratch directory (`-output-directory`)
//! using the source file's own directory as CWD, so relative
//! `\input`/`\includegraphics` references resolve as they would in a local
//! build. The scratch directory is removed before returning.
//!
//! ## Failure modes
//! - `ToolMissing` — `latexmk` is not on `PATH` (e.g. no TeX distribution).
//! - `Timeout` — compilation exceeded [`COMPILE_TIMEOUT_SECS`].
//! - `Failed { log }` — `latexmk` exited non-zero; the `.log` is captured so
//!   callers can surface a useful message (the file viewer falls back to plain
//!   text in this case).
//!
//! ## Residual limitations
//! - System TeX packages (under [`TEXMF_PREFIXES`]) are deliberately excluded
//!   from the dependency hash — they only change with a TeX distribution
//!   upgrade, which is rare and easy to handle by clearing the cache.
//! - Files consumed via `\input{|"shell command"}` (shell-escape) are not
//!   recorded in the `.fls`; documents relying on this will not invalidate
//!   the cache properly. Acceptable for V1.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::process::Command;

/// Hard ceiling for a single `latexmk` run. `latexmk` itself never prompts
/// under `-interaction=nonstopmode`, but packages can still hang (e.g. waiting
/// on missing fonts); the timeout guards against that.
const COMPILE_TIMEOUT_SECS: u64 = 30;

/// Default subdirectory of the OS temp dir used to store cached PDFs and
/// per-compile scratch directories.
const CACHE_DIR_NAME: &str = "skald-latex";

/// Extensions of files TeX produces as side-effects of compilation. They are
/// written to the output directory alongside the PDF and never count as
/// user-controlled dependencies.
const AUX_EXTS: &[&str] = &[
    "aux", "log", "fls", "fdb_latexmk", "synctex.gz", "out",
    "toc", "bbl", "blg", "run.xml", "idx", "ind", "ilg",
    "lof", "lot", "nav", "snm", "vrb", "bcf", "xdv", "mtc",
];

/// Path prefixes that identify a system TeX distribution. Files matched here
/// (e.g. `/usr/local/texlive/2024/texmf-dist/.../article.cls`) are filtered out
/// of the dependency set: they only change on a distro upgrade, which is rare
/// and easy to handle by clearing the cache manually.
const TEXMF_PREFIXES: &[&str] = &[
    "/usr/local/texlive",
    "/Library/TeX",
    "/opt/homebrew/texlive",
    "/usr/share/texmf",
    "/usr/share/texlive",
    "/var/lib/texmf",
];

/// A successfully compiled PDF.
pub struct CompiledPdf {
    pub bytes: Vec<u8>,
    /// `true` when served from cache without invoking `latexmk`. Currently
    /// informational only — surfaced in caller-side metrics/telemetry when
    /// needed; kept on the struct so the API stays stable.
    #[allow(dead_code)]
    pub from_cache: bool,
}

/// Why a compilation request did not yield a PDF.
#[derive(Debug)]
pub enum CompileError {
    /// `latexmk` is not reachable on `PATH`.
    ToolMissing,
    /// `latexmk` ran but exited with a non-zero status. Carries the textual
    /// `.log` (or a synthetic message when the log is unavailable).
    Failed { log: String },
    /// Compilation did not finish within [`COMPILE_TIMEOUT_SECS`].
    Timeout,
    /// Underlying I/O error (reading the source, writing the cache, etc.).
    Io(std::io::Error),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolMissing => write!(f, "latexmk is not available on the server"),
            Self::Failed { log } => write!(f, "compilation failed:\n{log}"),
            Self::Timeout => write!(f, "compilation aborted (timeout {COMPILE_TIMEOUT_SECS}s)"),
            Self::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl std::error::Error for CompileError {}

impl From<std::io::Error> for CompileError {
    fn from(e: std::io::Error) -> Self { Self::Io(e) }
}

/// Stateless-ish facade around `latexmk`. Owns only the cache root path; safe
/// to share via `Arc` (constructed once and stored on `Skald`).
#[derive(Clone)]
pub struct LatexCompiler {
    cache_dir: PathBuf,
}

impl LatexCompiler {
    pub fn new() -> Self {
        Self { cache_dir: std::env::temp_dir().join(CACHE_DIR_NAME) }
    }

    /// Compile `tex_path` into a PDF, serving from cache when possible.
    ///
    /// Cache lookup is **dependency-aware**: we first consult the `.fls`
    /// sidecar that records every input TeX read on the last compile of this
    /// source, hash the contents of those input files, and look up the PDF by
    /// that composite hash. This means a change to any `\input`'ed fragment,
    /// custom `.sty`, `.bib`, or `\includegraphics` target invalidates the
    /// cache correctly even when the main `.tex` file is unchanged. See the
    /// module docs for the full algorithm.
    pub async fn compile(&self, tex_path: &Path) -> Result<CompiledPdf, CompileError> {
        let path_key = path_hash(tex_path);
        let fls_sidecar = self.cache_dir.join(format!("{path_key}.fls"));

        // ── Cache lookup ───────────────────────────────────────────────────
        // Read the cached .fls from the last compile of this exact path; if
        // present, derive the composite deps hash and look for the PDF.
        if let Ok(fls_text) = tokio::fs::read_to_string(&fls_sidecar).await {
            let deps = parse_user_deps(&fls_text, tex_path);
            match composite_hash_of(&deps).await {
                Ok(deps_key) => {
                    let cached_pdf = self.cache_dir.join(format!("{deps_key}.pdf"));
                    if let Ok(bytes) = tokio::fs::read(&cached_pdf).await {
                        tracing::debug!(
                            ?cached_pdf, deps_count = deps.len(),
                            "latex cache hit (deps-aware)"
                        );
                        return Ok(CompiledPdf { bytes, from_cache: true });
                    }
                }
                Err(e) => {
                    // One of the recorded deps is missing/unreadable — most
                    // likely a `\input` was deleted. Treat as a miss and
                    // recompile, which will refresh the `.fls` sidecar.
                    tracing::debug!(
                        error = %e, sidecar = ?fls_sidecar,
                        "deps hashing failed — falling through to fresh compile"
                    );
                }
            }
        }

        // ── Cache miss → compile ───────────────────────────────────────────
        let (pdf_bytes, fresh_fls) = self.fresh_compile(tex_path, &path_key).await?;

        // Persist the new .fls sidecar (overwrites the previous one for this
        // path). A write failure is non-fatal: the next request will simply
        // recompile again.
        if let Err(e) = tokio::fs::write(&fls_sidecar, &fresh_fls).await {
            tracing::warn!(?fls_sidecar, error = %e, "fls sidecar write failed");
        }

        // Compute the composite hash from the freshly recorded deps and store
        // the PDF under that key.
        let deps = parse_user_deps(&fresh_fls, tex_path);
        let deps_key = composite_hash_of(&deps)
            .await
            .unwrap_or_else(|_| path_key.clone()); // fallback: at least cache by path
        let cached_pdf = self.cache_dir.join(format!("{deps_key}.pdf"));
        if let Err(e) = tokio::fs::write(&cached_pdf, &pdf_bytes).await {
            tracing::warn!(?cached_pdf, error = %e, "latex cache write failed");
        }

        tracing::info!(
            file = ?tex_path, deps_count = deps.len(),
            "latex compiled (cache miss)"
        );
        Ok(CompiledPdf { bytes: pdf_bytes, from_cache: false })
    }

    /// Paths that should be watched to detect any change affecting the compiled
    /// output of `tex_path`. Returns the source file itself plus every
    /// user-controlled dependency listed in the cached `.fls` sidecar (the
    /// recorder file from the last compile).
    ///
    /// Returns just `[tex_path]` when no `.fls` is cached yet (e.g. before the
    /// first compile), so the file watcher can install at least a baseline
    /// watcher — once the first compile happens and the `.fls` is written, the
    /// caller can call this again to pick up the full dependency set.
    ///
    /// This is a synchronous best-effort read: a missing or unreadable `.fls`
    /// is treated as "no deps known" rather than an error.
    pub fn watch_paths_for(&self, tex_path: &Path) -> Vec<PathBuf> {
        let mut paths = vec![tex_path.to_path_buf()];

        let path_key = path_hash(tex_path);
        let fls_sidecar = self.cache_dir.join(format!("{path_key}.fls"));

        if let Ok(fls_text) = std::fs::read_to_string(&fls_sidecar) {
            for dep in parse_user_deps(&fls_text, tex_path) {
                if !paths.contains(&dep) {
                    paths.push(dep);
                }
            }
        }

        paths
    }

    /// Run `latexmk` for `tex_path` and return both the produced PDF bytes
    /// and the textual `.fls` recorder file.
    ///
    /// Uses a per-hash scratch directory under [`CACHE_DIR_NAME`] as the
    /// `-output-directory`, while keeping the source file's directory as CWD so
    /// relative `\input`/`\includegraphics` references resolve normally. The
    /// scratch directory is removed before returning, regardless of outcome.
    ///
    /// `path_key` is used only to namespace the scratch directory; it does not
    /// affect the produced artefacts.
    async fn fresh_compile(
        &self,
        tex_path: &Path,
        path_key: &str,
    ) -> Result<(Vec<u8>, String), CompileError> {
        if find_on_path("latexmk").await.is_none() {
            return Err(CompileError::ToolMissing);
        }

        // Use a unique suffix so concurrent compiles of the same source (e.g.
        // two requests racing before the .fls sidecar is written) do not
        // collide on the scratch directory.
        let out_dir = self.cache_dir.join(format!("{path_key}-{}/", unique_suffix()));
        tokio::fs::create_dir_all(&out_dir).await?;

        // CWD = source file's directory; falls back to "." for unusual inputs.
        let cwd = tex_path.parent().unwrap_or_else(|| Path::new("."));

        let mut cmd = Command::new("latexmk");
        cmd.args([
            "-xelatex",
            "-interaction=nonstopmode",
            "-halt-on-error",
            "-file-line-error",
            "-recorder",           // ensure .fls is always produced
        ]);
        cmd.arg(format!("-output-directory={}", out_dir.display()));
        cmd.arg(tex_path);
        cmd.current_dir(cwd);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        // If our future is dropped (e.g. on shutdown) ensure the process dies.
        cmd.kill_on_drop(true);

        let output = match tokio::time::timeout(
            Duration::from_secs(COMPILE_TIMEOUT_SECS),
            cmd.output(),
        ).await {
            Ok(Ok(o))  => o,
            Ok(Err(e)) => {
                let _ = cleanup_dir(&out_dir).await;
                return Err(CompileError::Io(e));
            }
            Err(_) => {
                // Timeout: `cmd.output()` future is dropped here; `kill_on_drop`
                // takes care of terminating `latexmk`.
                let _ = cleanup_dir(&out_dir).await;
                return Err(CompileError::Timeout);
            }
        };

        if !output.status.success() {
            let log = read_compile_log(&out_dir, tex_path).await;
            let _ = cleanup_dir(&out_dir).await;
            return Err(CompileError::Failed { log });
        }

        let stem = file_stem(tex_path).unwrap_or_else(|| "output".to_string());
        let pdf_path = out_dir.join(format!("{stem}.pdf"));
        let fls_path = out_dir.join(format!("{stem}.fls"));

        let pdf_bytes = match tokio::fs::read(&pdf_path).await {
            Ok(b) => b,
            Err(e) => {
                let _ = cleanup_dir(&out_dir).await;
                return Err(CompileError::Failed {
                    log: format!("latexmk exited successfully but the PDF was not found ({e})"),
                });
            }
        };
        // The .fls should always exist under -recorder; degrade gracefully to
        // an empty string if missing — the deps-hash will then fall back to
        // the path-key, which still caches correctly for self-contained docs.
        let fls_text = tokio::fs::read_to_string(&fls_path).await.unwrap_or_default();

        let _ = cleanup_dir(&out_dir).await;
        Ok((pdf_bytes, fls_text))
    }
}

impl Default for LatexCompiler {
    fn default() -> Self { Self::new() }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// First 5 bytes (10 hex chars) of SHA-256 — enough to avoid collisions in
/// practice while keeping cache filenames short.
fn content_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

/// Short hash of the source file's absolute path. Used to find the `.fls`
/// sidecar that records the dependency list for that source. The path itself
/// (not its content) is hashed so the sidecar location is stable across
/// content edits.
fn path_hash(tex_path: &Path) -> String {
    // Canonicalise when possible so that `./foo.tex` and `/abs/foo.tex` resolve
    // to the same key. If the file does not exist yet we fall back to the raw
    // bytes — path_hash is only ever called for files we are about to compile,
    // so this branch is essentially unreachable in practice.
    let key: Vec<u8> = std::fs::canonicalize(tex_path)
        .map(|p| p.to_string_lossy().into_owned().into_bytes())
        .unwrap_or_else(|_| tex_path.to_string_lossy().into_owned().into_bytes());
    content_hash(&key)
}

/// Hash of every input file's contents, combined deterministically. Order is
/// stabilised by sorting the paths before hashing so reordering lines in the
/// `.fls` does not invalidate the cache.
///
/// Returns `Err` if any dependency cannot be read — callers should treat that
/// as a cache miss (a `\input` was probably deleted).
async fn composite_hash_of(deps: &[PathBuf]) -> std::io::Result<String> {
    let mut sorted: Vec<&PathBuf> = deps.iter().collect();
    sorted.sort();

    let mut hasher = Sha256::new();
    for dep in &sorted {
        let bytes = tokio::fs::read(dep).await?;
        // Include the path in the hash too: two swapped files with identical
        // contents (e.g. chapter1.tex ↔ chapter2.tex) would otherwise collide.
        hasher.update(dep.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update(&bytes);
        hasher.update(b"\0");
    }
    let digest = hasher.finalize();
    Ok(digest.iter().take(5).map(|b| format!("{b:02x}")).collect())
}

/// Parse a `.fls` recorder file and return the user-controlled input files.
///
/// The `.fls` format is a sequence of `INPUT <path>` and `OUTPUT <path>` lines
/// produced by TeX's `-recorder` flag (orchestrated here by `latexmk`). We
/// keep only the `INPUT` lines that:
///
/// - Are not part of the system TeX distribution (see [`TEXMF_PREFIXES`]);
/// - Are not generated artefacts (see [`AUX_EXTS`]);
/// - Do not live inside the scratch output directory produced during compile.
///
/// `tex_path` provides the CWD that `latexmk` was invoked from, so that
/// relative paths in the `.fls` (always relative to the CWD, not the source
/// file) can be resolved.
fn parse_user_deps(fls_text: &str, tex_path: &Path) -> Vec<PathBuf> {
    let cwd = tex_path.parent().unwrap_or_else(|| Path::new("."));
    let mut deps: Vec<PathBuf> = Vec::new();

    for line in fls_text.lines() {
        let path_str = match line.strip_prefix("INPUT ") {
            Some(p) => p.trim(),
            None => continue,
        };
        if path_str.is_empty() { continue; }

        // Resolve relative paths against the CWD that latexmk was invoked from.
        let raw = Path::new(path_str);
        let resolved: PathBuf = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            cwd.join(raw)
        };

        if !is_user_input(&resolved) { continue; }

        if !deps.contains(&resolved) {
            deps.push(resolved);
        }
    }
    deps
}

/// Decide whether a file recorded as an INPUT in the `.fls` is a
/// user-controlled dependency worth hashing. See [`TEXMF_PREFIXES`] and
/// [`AUX_EXTS`].
fn is_user_input(path: &Path) -> bool {
    let s = path.to_string_lossy();

    // Skip anything inside a known TeX distribution prefix.
    if TEXMF_PREFIXES.iter().any(|prefix| s.starts_with(prefix)) {
        return false;
    }

    // Skip aux/output artefacts by extension. Handle compound extensions like
    // `synctex.gz` by checking the last two segments joined by '.'.
    let single_ext = path.extension().and_then(|e| e.to_str());
    let compound_ext = path.file_name().and_then(|n| n.to_str()).and_then(|name| {
        let parts: Vec<&str> = name.split('.').collect();
        if parts.len() >= 2 {
            Some(format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1]))
        } else {
            None
        }
    });

    let candidates: [Option<&str>; 2] = [single_ext, compound_ext.as_deref()];
    for ext in candidates.into_iter().flatten() {
        if AUX_EXTS.iter().any(|aux| *aux == ext) {
            return false;
        }
    }

    true
}

/// Per-compile unique suffix (PID + nanosecond timestamp) to namespace the
/// scratch output directory and avoid races between concurrent compiles of the
/// same source.
fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{pid}-{nanos:x}")
}

/// Return the absolute path of `bin` if it is found on `PATH` and is a regular
/// file. We avoid pulling in the `which` crate for a single lookup.
async fn find_on_path(bin: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(bin);
        if tokio::fs::metadata(&candidate).await
            .map(|m| m.is_file() || m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Some(candidate);
        }
    }
    None
}

/// Read `latexmk`'s `.log` from the scratch directory, falling back to a
/// synthetic message if the log is missing or unreadable.
async fn read_compile_log(out_dir: &Path, tex_path: &Path) -> String {
    let stem = file_stem(tex_path).unwrap_or_else(|| "output".to_string());
    let log_path = out_dir.join(format!("{stem}.log"));
    tokio::fs::read_to_string(&log_path)
        .await
        .unwrap_or_else(|_| String::from("(no log file available)"))
}

/// Recursively remove a scratch directory. Errors are logged and swallowed:
/// leftover dirs only consume a little disk under the OS temp folder.
async fn cleanup_dir(dir: &Path) -> std::io::Result<()> {
    if tokio::fs::try_exists(dir).await.unwrap_or(false) {
        tokio::fs::remove_dir_all(dir).await?;
    }
    Ok(())
}

fn file_stem(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_10_lowercase_hex_chars() {
        let h = content_hash(b"hello world");
        assert_eq!(h.len(), 10);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn hash_is_deterministic() {
        assert_eq!(content_hash(b"abc"), content_hash(b"abc"));
        assert_ne!(content_hash(b"abc"), content_hash(b"abd"));
    }

    #[test]
    fn path_hash_is_stable_for_same_path() {
        let p = Path::new("/tmp/foo.tex");
        assert_eq!(path_hash(p), path_hash(p));
    }

    #[test]
    fn path_hash_differs_for_different_paths() {
        let a = path_hash(Path::new("/tmp/foo.tex"));
        let b = path_hash(Path::new("/tmp/bar.tex"));
        assert_ne!(a, b);
    }

    const SAMPLE_FLS: &str = "\
INPUT /usr/local/texlive/2024/texmf-dist/tex/latex/base/article.cls
INPUT chapters/intro.tex
INPUT chapters/intro.tex
INPUT images/diagram.pdf
INPUT refs.bib
INPUT custom.sty
INPUT /Library/TeX/texmf/tex/latex/amsmath/amsmath.sty
INPUT hello.aux
INPUT hello.fls
INPUT hello.synctex.gz
OUTPUT hello.pdf
OUTPUT hello.log
";

    #[test]
    fn parse_user_deps_filters_texmf_and_aux() {
        let deps = parse_user_deps(SAMPLE_FLS, Path::new("/project/hello.tex"));
        let mut got: Vec<String> = deps.into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        got.sort();

        // Only user-controlled inputs remain, deduped; relative paths resolve
        // against the .tex's parent directory.
        let mut expected = vec![
            "/project/chapters/intro.tex".to_string(),
            "/project/images/diagram.pdf".to_string(),
            "/project/refs.bib".to_string(),
            "/project/custom.sty".to_string(),
        ];
        expected.sort();

        assert_eq!(got, expected);
    }

    #[test]
    fn is_user_input_rejects_known_aux_extensions() {
        for ext in ["aux", "log", "fls", "fdb_latexmk", "toc", "bbl", "synctex.gz"] {
            let path_str = format!("/tmp/out/file.{ext}");
            let path = Path::new(&path_str);
            assert!(!is_user_input(path), "expected {ext} to be filtered out");
        }
    }

    #[test]
    fn is_user_input_keeps_user_files() {
        for ext in ["tex", "sty", "cls", "bib", "png", "jpg", "pdf", "eps"] {
            let path_str = format!("/project/file.{ext}");
            let path = Path::new(&path_str);
            assert!(is_user_input(path), "expected {ext} to be kept");
        }
    }

    #[test]
    fn is_user_input_rejects_texmf_paths() {
        for prefix in TEXMF_PREFIXES {
            let path_str = format!("{prefix}/2024/texmf-dist/foo.sty");
            let path = Path::new(&path_str);
            assert!(!is_user_input(path), "expected {prefix} to be filtered");
        }
    }

    #[tokio::test]
    async fn composite_hash_is_deterministic_for_same_contents() {
        // Use the test file itself as a stand-in dependency: it exists on disk
        // and its contents are stable for the duration of the test.
        let me = Path::new(file!());
        let deps_a = vec![me.to_path_buf()];
        let deps_b = vec![me.to_path_buf()];
        assert_eq!(
            composite_hash_of(&deps_a).await.unwrap(),
            composite_hash_of(&deps_b).await.unwrap()
        );
    }

    #[tokio::test]
    async fn composite_hash_is_order_independent() {
        let me = Path::new(file!());
        let cargo = Path::new("Cargo.toml");
        let a = vec![me.to_path_buf(), cargo.to_path_buf()];
        let b = vec![cargo.to_path_buf(), me.to_path_buf()];
        assert_eq!(
            composite_hash_of(&a).await.unwrap(),
            composite_hash_of(&b).await.unwrap()
        );
    }

    #[tokio::test]
    async fn composite_hash_fails_when_a_dep_is_missing() {
        let deps = vec![PathBuf::from("/this/path/does/not/exist.tex")];
        assert!(composite_hash_of(&deps).await.is_err());
    }

    #[test]
    fn watch_paths_for_returns_just_tex_when_no_fls_cached() {
        // Point the compiler at an empty cache directory so no .fls exists.
        let compiler = LatexCompiler { cache_dir: PathBuf::from("/tmp/skald-latex-test-empty") };
        let tex = Path::new("/project/hello.tex");
        let paths = compiler.watch_paths_for(tex);
        assert_eq!(paths, vec![tex]);
    }

    #[test]
    fn watch_paths_for_includes_deps_when_fls_is_present() {
        // Write a .fls sidecar at the cache location watch_paths_for consults.
        // The sidecar's path is derived from path_hash(tex), which we compute
        // via the public surface by re-using the same helper.
        let tmp = std::env::temp_dir().join(format!("skald-latex-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let tex = std::env::current_dir().unwrap().join("Cargo.toml"); // arbitrary existing file
        let tex_canonical = std::fs::canonicalize(&tex).unwrap();
        let path_key = {
            let mut h = Sha256::new();
            h.update(tex_canonical.to_string_lossy().as_bytes());
            let d = h.finalize();
            d.iter().take(5).map(|b| format!("{b:02x}")).collect::<String>()
        };
        let fls_path = tmp.join(format!("{path_key}.fls"));
        std::fs::write(&fls_path, format!(
            "INPUT /usr/local/texlive/2024/texmf-dist/tex/latex/base/article.cls\n\
             INPUT chapters/intro.tex\n\
             INPUT custom.sty\n\
             INPUT hello.aux\n\
             OUTPUT hello.pdf\n\
            ")
        ).unwrap();

        let compiler = LatexCompiler { cache_dir: tmp.clone() };
        let paths = compiler.watch_paths_for(&tex_canonical);

        // tex itself + the two user-controlled deps (intro.tex, custom.sty).
        // The texmf path and the .aux are filtered out.
        assert!(paths.contains(&tex_canonical));
        let intro = tex_canonical.parent().unwrap().join("chapters/intro.tex");
        let sty = tex_canonical.parent().unwrap().join("custom.sty");
        assert!(paths.contains(&intro), "missing {intro:?} in {paths:?}");
        assert!(paths.contains(&sty), "missing {sty:?} in {paths:?}");
        assert_eq!(paths.len(), 3);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
