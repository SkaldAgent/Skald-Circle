use std::path::Path;

use axum::{
    Extension, Json,
    body::{Body, Bytes},
    extract::{Query, State},
    http::{HeaderValue, HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use std::sync::Arc;
use core_api::user_fs::UserFs;
use skald_core::db::memory_docs;
use skald_core::session::handler::media;
use skald_core::skald::Skald;
use skald_core::latex::CompileError;
use skald_core::tools::fs as fs_tools;
use super::ApiError;
use super::guard::AuthUser;
use super::require_context;

/// Upload body cap for `POST /api/file/upload` (same budget as chat attachments).
pub const MAX_UPLOAD_BYTES: usize = 256 * 1024 * 1024;

#[derive(Serialize)]
pub struct FileEntry {
    pub path: String,
    pub name: String,
}

/// One row of a directory listing: name + agent path (round-trips through
/// `/api/file`) + the metadata the explorer table shows. `size` is files-only;
/// timestamps are RFC-3339 UTC (`None` when the filesystem can't provide them,
/// e.g. no birth-time support) and formatted client-side.
#[derive(Serialize)]
pub struct DirEntry {
    pub name:        String,
    pub path:        String,
    pub is_dir:      bool,
    pub size:        Option<u64>,
    pub created_at:  Option<String>,
    pub modified_at: Option<String>,
}

fn fmt_ts(t: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339()
}

/// Reject a write when the caller's mount for this path is read-only
/// (a shared-folder / project membership without `can_write`, or the docs
/// tree). The container bind mount is the physical gate for in-container
/// writes; the host-side HTTP API needs its own check.
fn require_write(fs: &UserFs, agent: &str) -> Result<(), ApiError> {
    if fs.can_write_to(agent) {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!("read-only: {agent}")))
    }
}

/// A lightweight version token for a file on disk, used as a strong-ish `ETag`
/// for the editor's optimistic locking. It is *not* a content hash: it combines
/// the mtime (nanosecond precision on the local filesystems we run on) and the
/// size, which is enough to detect "someone wrote after you loaded" without the
/// cost of hashing every served file (including images/PDFs). Two distinct
/// writes with identical size in the same nanosecond would collide — acceptable
/// for a small-instance, last-write-wins-becomes-visible contract.
fn disk_etag(md: &std::fs::Metadata) -> String {
    let mtime_ns = md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("\"{}-{}\"", mtime_ns, md.len())
}

/// GET /api/files/dir?path=… — the immediate children of a directory (dirs
/// first, then name), resolved and scoped exactly like `GET /api/file`.
pub async fn list_dir(
    State(state):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Query(q):        Query<FileQuery>,
) -> Result<Json<Vec<DirEntry>>, ApiError> {
    let ctx = require_context(&state, &auth.user_id).await?;
    let (abs, agent) = fs_tools::resolve_view_path(ctx.fs.load().as_ref(), &q.path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    if !abs.is_dir() {
        return Err(ApiError::bad_request(format!("not a directory: {agent}")));
    }
    let mut entries: Vec<DirEntry> = Vec::new();
    for entry in std::fs::read_dir(&abs)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let md = entry.metadata().ok();
        let is_dir = md.as_ref().is_some_and(|m| m.is_dir());
        entries.push(DirEntry {
            path: format!("{agent}/{name}"),
            name,
            is_dir,
            size:        md.as_ref().filter(|m| m.is_file()).map(|m| m.len()),
            created_at:  md.as_ref().and_then(|m| m.created().ok()).map(fmt_ts),
            modified_at: md.as_ref().and_then(|m| m.modified().ok()).map(fmt_ts),
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(Json(entries))
}

// ── Directory download (streaming ZIP) ─────────────────────────────────────

/// GET /api/file/download?path=… — stream a directory to the browser as a ZIP
/// attachment. The archive is built on the fly: an async task walks the tree
/// and an async ZIP writer streams entries into a bounded duplex stream that
/// backs the response body. No temp file, no whole-archive buffer; the bounded
/// pipe gives backpressure, and a client disconnect errors the writer, ending
/// the task. Single files are *not* served here — `GET /api/file` with
/// `force_download=true` already does that.
///
/// Compression is decided per entry: Deflate at maximum level, except files
/// whose magic bytes already name a compressed container (images/video/PDF,
/// the ZIP family, gzip/zstd/7z/rar, compressed audio) — those are Stored,
/// since re-deflating them only burns CPU. Unix permission bits are preserved.
pub async fn download_dir(
    State(state):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Query(q):        Query<FileQuery>,
) -> Result<Response, ApiError> {
    let ctx = require_context(&state, &auth.user_id).await?;
    let fs = ctx.fs.load();
    let (abs, agent) = fs_tools::resolve_view_path(fs.as_ref(), &q.path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    if !abs.is_dir() {
        return Err(ApiError::bad_request(format!(
            "not a directory: {agent} — single files download via GET /api/file?force_download=true"
        )));
    }
    let base = abs
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());

    let zip_name = format!("{base}.zip");
    let (writer, reader) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        if let Err(e) = write_zip(writer, abs, base).await {
            tracing::warn!(error = ?e, "zip download aborted");
        }
    });
    let mut response = Response::new(Body::from_stream(tokio_util::io::ReaderStream::new(reader)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    set_attachment(&mut response, &zip_name);
    Ok(response)
}

/// Walk `root` and write the whole tree into `sink` as a streaming ZIP whose
/// entries are named `{base}/{relative}`. The writer drives a bounded duplex
/// stream, so it suspends when the client falls behind and errors out when the
/// client goes away, instead of building an archive nobody is reading.
/// Containment mirrors `resolve_host_path`, fail-closed: every entry is
/// canonicalized and must stay under `root`, and symlinks are never followed
/// into the archive (an in-tree one would duplicate its target, an escaping
/// one would leave the workspace). A file that vanishes or turns unreadable
/// mid-walk is skipped with a warning: the folder is live (the agent may be
/// writing in it), so the archive is best-effort, not atomic.
async fn write_zip(
    sink:      tokio::io::DuplexStream,
    root:      std::path::PathBuf,
    base:      String,
) -> anyhow::Result<()> {
    use async_zip::{AttributeCompatibility, Compression, DeflateOption, ZipEntryBuilder};
    use futures::io::AsyncWriteExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    use tokio::io::AsyncReadExt as _;

    let mut zip = async_zip::base::write::ZipFileWriter::with_tokio(sink);
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = match std::fs::read_dir(&dir) {
            Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
            Err(e) => {
                tracing::warn!(path = %dir.display(), error = %e, "zip download: skipping unreadable directory");
                continue;
            }
        };
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_symlink() || (!ft.is_dir() && !ft.is_file()) {
                continue;
            }
            let canon = match path.canonicalize() {
                Ok(c) if c.starts_with(&root) => c,
                _ => continue,
            };
            let rel = canon.strip_prefix(&root)?;
            let name = format!("{base}/{}", rel.to_string_lossy().replace('\\', "/"));
            let mode = entry.metadata().map(|m| m.permissions().mode()).unwrap_or(0o644) & 0o777;
            if ft.is_dir() {
                let ze = ZipEntryBuilder::new(format!("{name}/").into(), Compression::Stored)
                    .attribute_compatibility(AttributeCompatibility::Unix)
                    .unix_permissions(mode as u16);
                zip.write_entry_whole(ze, b"").await?;
                stack.push(path);
                continue;
            }
            let mut file = match tokio::fs::File::open(&canon).await {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(path = %canon.display(), error = %e, "zip download: skipping unreadable file");
                    continue;
                }
            };
            let mut head = [0u8; 16];
            let n = file.read(&mut head).await.unwrap_or(0);
            let compressible = !already_compressed(&head[..n], &ext_of(&canon));
            let compression = if compressible { Compression::Deflate } else { Compression::Stored };
            let mut ze = ZipEntryBuilder::new(name.into(), compression)
                .attribute_compatibility(AttributeCompatibility::Unix)
                .unix_permissions(mode as u16);
            if compressible {
                ze = ze.deflate_option(DeflateOption::Other(9));
            }
            let mut entry_writer = zip.write_entry_stream(ze).await?;
            entry_writer.write_all(&head[..n]).await?;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 { break; }
                entry_writer.write_all(&buf[..n]).await?;
            }
            entry_writer.close().await?;
        }
    }
    zip.close().await?;
    Ok(())
}

/// True when the first bytes of a file name a format that is already
/// compressed, so Deflate would only cost CPU. Magic bytes decide (robust for
/// extension-less files): the shared media sniffer covers images/video/PDF,
/// the explicit magics the archive and audio families, and the extension is
/// the last-resort fallback for containers without a distinctive header.
fn already_compressed(head: &[u8], ext: &str) -> bool {
    const MAGICS: &[&[u8]] = &[
        b"PK\x03\x04",         // ZIP family: zip/jar/apk/epub/docx/xlsx/odt…
        b"\x1f\x8b",           // gzip
        b"\x28\xb5\x2f\xfd",   // zstd
        b"7z\xbc\xaf\x27\x1c", // 7z
        b"Rar!\x1a\x07",       // rar
        b"BZh",                // bzip2
        b"\xfd7zXZ\x00",       // xz
        b"ID3",                // mp3 (tagged)
        b"OggS",               // ogg
        b"fLaC",               // flac
    ];
    if MAGICS.iter().any(|m| head.starts_with(m)) {
        return true;
    }
    // Untagged mp3 frame sync.
    if head.len() >= 2 && head[0] == 0xff && (head[1] & 0xe0) == 0xe0 {
        return true;
    }
    if media::sniff_mime(head).is_some() {
        return true;
    }
    matches!(ext, "heic" | "heif" | "avif" | "m4a" | "wma" | "ape" | "wv")
}

/// Lowercase extension for the compression heuristic.
fn ext_of(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

pub async fn list_files(
    State(state):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<FileEntry>>, ApiError> {
    // Scoped to the caller's own home; entries are returned as agent paths (`~/…`)
    // so they round-trip through `GET /api/file` unchanged.
    let ctx = require_context(&state, &auth.user_id).await?;
    let root = ctx.fs.load().home_host.clone();
    let mut paths: Vec<String> = Vec::new();
    walk(&root, &root, &mut paths)?;
    paths.sort();

    let entries = paths
        .into_iter()
        .map(|rel| {
            let name = Path::new(&rel)
                .file_stem()
                .map_or_else(|| rel.clone(), |s| s.to_string_lossy().to_string());
            FileEntry { path: format!("~/{rel}"), name }
        })
        .collect();
    Ok(Json(entries))
}

#[derive(Deserialize)]
pub struct FileQuery {
    pub path: String,
    /// When `true` and `path` points at a `.tex` / `.latex` file, compile it
    /// to PDF via `latexmk` and return the PDF bytes instead of the raw
    /// source. Other file types ignore this flag.
    #[serde(rename = "compile-latex", default)]
    pub compile_latex: bool,
    /// When `true`, mark the response as a download (`Content-Disposition:
    /// attachment`) so the browser saves the file instead of rendering it
    /// inline. For a compiled `.tex` the attachment name is `<stem>.pdf`.
    #[serde(rename = "force_download", default)]
    pub force_download: bool,
}

/// Serve a file's raw bytes with a `Content-Type` derived from its extension.
///
/// Raw bytes (not `read_to_string`) so binary formats — images, PDFs — work; the
/// frontend file viewer reads text via `res.text()` and binaries via `res.blob()`.
///
/// With `?compile-latex=true` a `.tex` source is compiled to PDF (see
/// [`skald_core::latex::LatexCompiler`]); the response is then
/// `application/pdf`. Compilation failures yield `422 Unprocessable Entity`
/// with the textual `latexmk` log in the body, so the caller can fall back to
/// showing the raw source.
///
/// A path under a virtual memory root (`user-memory/…`, `shared-memory/…`) is
/// served from the `memory_docs` table — the caller's own pool for the private
/// root, the system pool for the shared one — exactly like the fs-tools route
/// them (see [`fs_tools::classify_memory`]). Raw content only: no LaTeX
/// compilation (notes are not on disk).
pub async fn get_file(
    State(state):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Query(q):        Query<FileQuery>,
) -> Response {
    let ctx = match require_context(&state, &auth.user_id).await {
        Ok(c)  => c,
        Err(e) => return e.into_response(),
    };

    // Virtual memory namespace → SQLite, not disk.
    if let Some(mem) = fs_tools::classify_memory(&q.path) {
        let pool = match mem.scope {
            fs_tools::MemScope::User   => Arc::clone(&ctx.pool),
            fs_tools::MemScope::Shared => state.db().clone(),
        };
        return match memory_docs::get(&pool, &mem.rel).await {
            Ok(Some(doc)) => {
                let mut response = doc.content.into_response();
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(content_type_for(&q.path)),
                );
                if q.force_download {
                    set_attachment(&mut response, &basename(&q.path));
                }
                response
            }
            Ok(None)  => (StatusCode::NOT_FOUND, format!("File not found: {}", q.path)).into_response(),
            Err(e)    => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        };
    }

    let user_fs = ctx.fs.load();
    let (target, agent) = match fs_tools::resolve_view_target(user_fs.as_ref(), &q.path) {
        Ok(resolved) => resolved,
        Err(e)       => return (StatusCode::BAD_REQUEST, format!("Invalid path: {e}")).into_response(),
    };
    let writable = user_fs.can_write_to(&agent);

    // A container-only path (`/tmp/…`) has no host file behind it: the bytes come
    // out through the container, so the user sees what the agent read. Served
    // read-only — the editor's optimistic locking is an on-disk `mtime`+`len`,
    // which has no counterpart here, and without an ETag the frontend keeps the
    // file in view mode rather than risking a blind overwrite.
    let abs = match target {
        fs_tools::FsTarget::Host(abs) => abs,
        fs_tools::FsTarget::Container { container, path } => {
            return match skald_core::container::exec_fs::read(&container, &path).await {
                Ok(bytes) => {
                    let mut response = bytes.into_response();
                    response.headers_mut().insert(
                        header::CONTENT_TYPE,
                        HeaderValue::from_static(content_type_for(&q.path)),
                    );
                    if q.force_download {
                        set_attachment(&mut response, &basename(&q.path));
                    }
                    response
                }
                Err(_) => (StatusCode::NOT_FOUND, format!("File not found: {}", q.path))
                    .into_response(),
            };
        }
    };

    if q.compile_latex && is_latex(&q.path) {
        return match state.latex_compiler().compile(&abs).await {
            Ok(pdf) => {
                let mut response = pdf_response(pdf.bytes);
                if q.force_download {
                    set_attachment(&mut response, &pdf_download_name(&q.path));
                }
                response
            }
            Err(err) => compile_error_response(err),
        };
    }

    match tokio::fs::read(&abs).await {
        Ok(bytes) => {
            let mut response = bytes.into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type_for(&q.path)),
            );
            // Optimistic-locking version token + write flag for the editor.
            // Only disk files are editable through this surface, so both come
            // from the on-disk metadata / `UserFs` membership snapshot.
            if let Ok(md) = tokio::fs::metadata(&abs).await {
                if let Ok(v) = HeaderValue::from_str(&disk_etag(&md)) {
                    response.headers_mut().insert(header::ETAG, v);
                }
            }
            if writable {
                response.headers_mut().insert(
                    header::HeaderName::from_static("x-writable"),
                    HeaderValue::from_static("1"),
                );
            }
            if q.force_download {
                set_attachment(&mut response, &basename(&q.path));
            }
            response
        }
        Err(_) => (StatusCode::NOT_FOUND, format!("File not found: {}", q.path)).into_response(),
    }
}

/// Mark a response as a browser download via `Content-Disposition: attachment`.
///
/// HTTP header values must be visible ASCII, so the filename is sanitised
/// (quotes, backslashes and non-ASCII bytes become `_`). This keeps it
/// dependency-free; the worst case for an exotic filename is a couple of `_`.
fn set_attachment(response: &mut Response, filename: &str) {
    let safe: String = filename
        .chars()
        .map(|c| if c.is_ascii() && c != '"' && c != '\\' { c } else { '_' })
        .collect();
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")) {
        response.headers_mut().insert(header::CONTENT_DISPOSITION, value);
    }
}

/// Final path component, e.g. `docs/report.tex` → `report.tex`.
fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().to_string())
}

/// Download name for a compiled LaTeX source: the stem with a `.pdf` extension,
/// e.g. `docs/report.tex` → `report.pdf`.
fn pdf_download_name(path: &str) -> String {
    let stem = Path::new(path)
        .file_stem()
        .map_or_else(|| "output".to_string(), |s| s.to_string_lossy().to_string());
    format!("{stem}.pdf")
}

/// Build a `200 OK` response carrying PDF bytes with the canonical
/// `application/pdf` content type and inline disposition.
fn pdf_response(bytes: Vec<u8>) -> Response {
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/pdf"),
    );
    response
}

/// Map a [`CompileError`] to an HTTP status that lets the frontend react:
/// `ToolMissing` → `501 Not Implemented`, `Timeout` → `504 Gateway Timeout`,
/// `Failed` → `422 Unprocessable Entity` (body = log), `Io` → `500`.
///
/// The body is always plain text so the viewer can show it directly.
fn compile_error_response(err: CompileError) -> Response {
    let (status, body): (StatusCode, String) = match err {
        CompileError::ToolMissing => (
            StatusCode::NOT_IMPLEMENTED,
            "latexmk is not installed on the server.".to_string(),
        ),
        CompileError::Timeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "LaTeX compilation aborted due to timeout.".to_string(),
        ),
        CompileError::Failed { log } => (StatusCode::UNPROCESSABLE_ENTITY, log),
        CompileError::Io(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("I/O error during compilation: {e}"),
        ),
    };
    let mut response = body.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    (status, response).into_response()
}

/// True for `.tex` / `.latex` extensions — i.e. inputs worth compiling.
fn is_latex(path: &str) -> bool {
    matches!(
        Path::new(path).extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("tex") | Some("latex")
    )
}

/// Best-effort `Content-Type` from a file extension. Known binary types get their
/// specific MIME; everything else is served as UTF-8 text (markdown, code, configs,
/// and unknown files the viewer treats as plain text or "binary, no preview").
fn content_type_for(path: &str) -> &'static str {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png"          => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif"          => "image/gif",
        "webp"         => "image/webp",
        "avif"         => "image/avif",
        "bmp"          => "image/bmp",
        "ico"          => "image/x-icon",
        "svg"          => "image/svg+xml",
        "pdf"          => "application/pdf",
        "tex" | "latex" => "application/x-tex",
        "html" | "htm" => "text/html; charset=utf-8",
        _              => "text/plain; charset=utf-8",
    }
}

#[derive(Deserialize)]
pub struct SavePayload {
    pub path:    String,
    pub content: String,
    /// Optional optimistic-locking token (the `ETag` returned by `GET /api/file`
    /// when the editor loaded the file). When present, the save only succeeds
    /// if the file on disk still matches; otherwise the handler returns
    /// `409 Conflict` so the editor can prompt (reload remote / overwrite /
    /// copy my changes) instead of silently clobbering a concurrent write.
    /// Absent ⇒ legacy last-write-wins behaviour (no caller is broken).
    #[serde(default)]
    pub if_match: Option<String>,
}

#[derive(Deserialize)]
pub struct CreatePayload {
    pub path: String,
    /// When `true`, create a directory instead of an empty file.
    #[serde(default)]
    pub dir: bool,
}

pub async fn create_file(
    State(state):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body):      Json<CreatePayload>,
) -> Result<StatusCode, ApiError> {
    let ctx = require_context(&state, &auth.user_id).await?;
    let fs = ctx.fs.load();
    let (abs, display) = fs_tools::resolve_view_path(fs.as_ref(), &body.path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    require_write(&fs, &display)?;
    if abs.exists() {
        return Err(anyhow::anyhow!("File already exists: {display}").into());
    }
    if body.dir {
        std::fs::create_dir_all(&abs)?;
    } else {
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&abs, "")?;
    }
    Ok(StatusCode::CREATED)
}

/// POST /api/file/upload?path=… — raw request-body bytes written to `path`
/// (create or replace), for binary uploads from the project explorer. The
/// route caps the body at [`MAX_UPLOAD_BYTES`]; parent dirs are created.
pub async fn upload_file(
    State(state):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Query(q):        Query<FileQuery>,
    body:            Bytes,
) -> Result<StatusCode, ApiError> {
    let ctx = require_context(&state, &auth.user_id).await?;
    let fs = ctx.fs.load();
    let (abs, display) = fs_tools::resolve_view_path(fs.as_ref(), &q.path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    require_write(&fs, &display)?;
    if abs.is_dir() {
        return Err(ApiError::bad_request(format!("is a directory: {display}")));
    }
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&abs, &body)?;
    Ok(StatusCode::CREATED)
}

pub async fn save_file(
    State(state):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body):      Json<SavePayload>,
) -> Result<(StatusCode, HeaderMap), ApiError> {
    let ctx = require_context(&state, &auth.user_id).await?;
    let fs = ctx.fs.load();
    let (abs, display) = fs_tools::resolve_view_path(fs.as_ref(), &body.path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    require_write(&fs, &display)?;
    if !abs.exists() {
        return Err(anyhow::anyhow!("File not found: {display}").into());
    }
    // Optimistic locking: if the caller pinned a version, refuse to overwrite a
    // file that changed underneath it. We treat a missing file here as a
    // conflict too (the base it was edited against is gone).
    if let Some(expected) = body.if_match.as_deref() {
        let current = std::fs::metadata(&abs).ok().map(|m| disk_etag(&m));
        if current.as_deref() != Some(expected) {
            return Err(ApiError::conflict(format!(
                "File modified remotely: {display}"
            )));
        }
    }
    std::fs::write(&abs, &body.content)?;
    // Echo the new version so the editor can update its token without a re-fetch.
    let mut headers = HeaderMap::new();
    if let Ok(md) = std::fs::metadata(&abs) {
        if let Ok(v) = HeaderValue::from_str(&disk_etag(&md)) {
            headers.insert(header::ETAG, v);
        }
    }
    Ok((StatusCode::NO_CONTENT, headers))
}

#[derive(Deserialize)]
pub struct RenamePayload {
    pub old_path: String,
    pub new_path: String,
}

pub async fn rename_file(
    State(state):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body):      Json<RenamePayload>,
) -> Result<StatusCode, ApiError> {
    let ctx = require_context(&state, &auth.user_id).await?;
    let fs = ctx.fs.load();
    let (old_abs, old_disp) = fs_tools::resolve_view_path(fs.as_ref(), &body.old_path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let (new_abs, new_disp) = fs_tools::resolve_view_path(fs.as_ref(), &body.new_path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    require_write(&fs, &old_disp)?;
    require_write(&fs, &new_disp)?;
    if !old_abs.exists() {
        return Err(anyhow::anyhow!("File not found: {old_disp}").into());
    }
    if new_abs.exists() {
        return Err(anyhow::anyhow!("File already exists: {new_disp}").into());
    }
    if let Some(parent) = new_abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&old_abs, &new_abs)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_file(
    State(state):    State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Query(q):        Query<FileQuery>,
) -> Result<StatusCode, ApiError> {
    let ctx = require_context(&state, &auth.user_id).await?;
    let fs = ctx.fs.load();
    let (abs, display) = fs_tools::resolve_view_path(fs.as_ref(), &q.path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    require_write(&fs, &display)?;
    if !abs.exists() {
        return Err(anyhow::anyhow!("File not found: {display}").into());
    }
    if abs.is_dir() {
        std::fs::remove_dir_all(&abs)?;
    } else {
        std::fs::remove_file(&abs)?;
    }
    Ok(StatusCode::NO_CONTENT)
}

fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) -> anyhow::Result<()> {
    if !dir.exists() { return Ok(()); }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if matches!(name, ".git" | "target" | "node_modules") { continue; }
            walk(root, &path, out)?;
        } else if path.is_file() {
            let rel = path.strip_prefix(root)?.to_string_lossy().to_string();
            out.push(rel);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Streams a small on-disk tree through `write_zip` and reads the archive
    /// back with the crate's own reader: entry names (folder-as-prefix), file
    /// contents, per-entry compression (Stored for a fake PNG, Deflate for
    /// text), the empty directory surviving, and a symlink being skipped.
    #[tokio::test]
    async fn write_zip_round_trip_streams_the_whole_tree() {
        let dir = std::env::temp_dir().join(format!("skald-zip-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("nested/empty")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"hello hello hello hello hello").unwrap();
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&[7u8; 64]);
        std::fs::write(dir.join("nested/pic.png"), &png).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("notes.txt", dir.join("link.txt")).unwrap();

        // Production callers hand in a canonicalized root (resolve_view_path);
        // temp_dir() isn't one on macOS (/var → /private/var), so mirror that.
        let root = dir.canonicalize().unwrap();
        let (writer, mut reader) = tokio::io::duplex(64 * 1024);
        let write_task = tokio::spawn(async move { write_zip(writer, root, "pkg".to_string()).await });
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut bytes).await.unwrap();
        write_task.await.unwrap().unwrap();

        let zr = async_zip::base::read::mem::ZipFileReader::new(bytes).await.unwrap();
        let entries = zr.file().entries();
        let names: Vec<String> = entries
            .iter()
            .map(|e| e.filename().as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"pkg/notes.txt".to_string()), "{names:?}");
        assert!(names.contains(&"pkg/nested/pic.png".to_string()), "{names:?}");
        assert!(names.contains(&"pkg/nested/empty/".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("link.txt")), "{names:?}");

        for (i, e) in entries.iter().enumerate() {
            match e.filename().as_str().unwrap() {
                "pkg/notes.txt" => {
                    assert!(matches!(e.compression(), async_zip::Compression::Deflate));
                    let mut data = Vec::new();
                    let mut rd = zr.reader_without_entry(i).await.unwrap();
                    futures::io::AsyncReadExt::read_to_end(&mut rd, &mut data).await.unwrap();
                    assert_eq!(data, b"hello hello hello hello hello");
                }
                "pkg/nested/pic.png" => {
                    assert!(matches!(e.compression(), async_zip::Compression::Stored));
                    let mut data = Vec::new();
                    let mut rd = zr.reader_without_entry(i).await.unwrap();
                    futures::io::AsyncReadExt::read_to_end(&mut rd, &mut data).await.unwrap();
                    assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n");
                    assert_eq!(data.len(), 72);
                }
                _ => {}
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn already_compressed_sniffs_magic_and_extension() {
        assert!(already_compressed(b"PK\x03\x04rest", ""));
        assert!(already_compressed(b"\x1f\x8brest", ""));
        assert!(already_compressed(b"\x89PNG\r\n\x1a\nrest", ""));
        assert!(already_compressed(b"ID3rest", ""));
        assert!(already_compressed(b"plain text here", "heic"));
        assert!(!already_compressed(b"plain text here", "txt"));
        assert!(!already_compressed(b"", "md"));
    }
}
