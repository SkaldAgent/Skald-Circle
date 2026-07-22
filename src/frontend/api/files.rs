use std::path::Path;

use axum::{
    Extension, Json,
    body::Bytes,
    extract::{Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use std::sync::Arc;
use core_api::user_fs::UserFs;
use skald_core::db::memory_docs;
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
    let abs = match fs_tools::resolve_view_path(user_fs.as_ref(), &q.path) {
        Ok((abs, _)) => abs,
        Err(e)       => return (StatusCode::BAD_REQUEST, format!("Invalid path: {e}")).into_response(),
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
) -> Result<StatusCode, ApiError> {
    let ctx = require_context(&state, &auth.user_id).await?;
    let fs = ctx.fs.load();
    let (abs, display) = fs_tools::resolve_view_path(fs.as_ref(), &body.path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    require_write(&fs, &display)?;
    if !abs.exists() {
        return Err(anyhow::anyhow!("File not found: {display}").into());
    }
    std::fs::write(&abs, &body.content)?;
    Ok(StatusCode::NO_CONTENT)
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
