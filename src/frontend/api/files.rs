use std::path::Path;

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use std::sync::Arc;
use crate::core::skald::Skald;
use crate::core::latex::CompileError;
use crate::core::tools::fs as fs_tools;
use super::ApiError;

#[derive(Serialize)]
pub struct FileEntry {
    pub path: String,
    pub name: String,
}

pub async fn list_files(State(_state): State<Arc<Skald>>) -> Result<Json<Vec<FileEntry>>, ApiError> {
    let root = fs_tools::resolve(".")?;
    let mut paths: Vec<String> = Vec::new();
    walk(&root, &root, &mut paths)?;
    paths.sort();

    let entries = paths
        .into_iter()
        .map(|p| {
            let name = Path::new(&p)
                .file_stem()
                .map_or_else(|| p.clone(), |s| s.to_string_lossy().to_string());
            FileEntry { path: p, name }
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
/// [`crate::core::latex::LatexCompiler`]); the response is then
/// `application/pdf`. Compilation failures yield `422 Unprocessable Entity`
/// with the textual `latexmk` log in the body, so the caller can fall back to
/// showing the raw source.
pub async fn get_file(
    State(state): State<Arc<Skald>>,
    Query(q): Query<FileQuery>,
) -> Response {
    let abs = match fs_tools::resolve(&q.path) {
        Ok(p)  => p,
        Err(_) => return (StatusCode::BAD_REQUEST, format!("Invalid path: {}", q.path)).into_response(),
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
}

pub async fn create_file(
    State(_state): State<Arc<Skald>>,
    Json(body): Json<CreatePayload>,
) -> Result<StatusCode, ApiError> {
    let abs = fs_tools::resolve(&body.path)?;
    if abs.exists() {
        return Err(anyhow::anyhow!("File già esistente: {}", body.path).into());
    }
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&abs, "")?;
    Ok(StatusCode::CREATED)
}

pub async fn save_file(
    State(_state): State<Arc<Skald>>,
    Json(body): Json<SavePayload>,
) -> Result<StatusCode, ApiError> {
    let abs = fs_tools::resolve(&body.path)?;
    if !abs.exists() {
        return Err(anyhow::anyhow!("File not found: {}", body.path).into());
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
    State(_state): State<Arc<Skald>>,
    Json(body): Json<RenamePayload>,
) -> Result<StatusCode, ApiError> {
    let old_abs = fs_tools::resolve(&body.old_path)?;
    let new_abs = fs_tools::resolve(&body.new_path)?;
    if !old_abs.exists() {
        return Err(anyhow::anyhow!("File non trovato: {}", body.old_path).into());
    }
    if new_abs.exists() {
        return Err(anyhow::anyhow!("File già esistente: {}", body.new_path).into());
    }
    if let Some(parent) = new_abs.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&old_abs, &new_abs)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_file(
    State(_state): State<Arc<Skald>>,
    Query(q): Query<FileQuery>,
) -> Result<StatusCode, ApiError> {
    let abs = fs_tools::resolve(&q.path)?;
    if !abs.exists() {
        return Err(anyhow::anyhow!("File non trovato: {}", q.path).into());
    }
    std::fs::remove_file(&abs)?;
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
