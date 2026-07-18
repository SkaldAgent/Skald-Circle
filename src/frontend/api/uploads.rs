use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use axum::{
    Json, Extension,
    extract::{Multipart, Path, State},
};
use tokio::io::AsyncWriteExt;

use core_api::message_meta::Attachment;

use skald_core::session::handler::media::sniff_mime;
use skald_core::skald::Skald;
use skald_core::tools::fs as fs_tools;
use super::{ApiError, guard::AuthUser, require_context};
use super::sessions::SourcePath;

/// Max bytes accepted for a single uploaded file; anything larger is cut off
/// mid-stream, the partial file removed, and the request answered 413.
const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// `POST /api/{source}/uploads`
///
/// Accepts a `multipart/form-data` body with one or more file fields and saves
/// each under `data/uploads/{user_id}/{session_id}/` (per-user namespaced, so
/// colliding session ids across users never share a directory). Bytes are
/// streamed straight to disk (`field.chunk()` → file), never buffered whole in
/// RAM — the route disables the default body-size limit (see router) and
/// enforces [`MAX_UPLOAD_BYTES`] itself. When the magic bytes are recognized,
/// the sniffed MIME wins over the client-supplied `Content-Type`.
///
/// Returns the saved [`Attachment`]s (project-root-relative path, name, MIME,
/// size) so the client can show chips and echo them back when sending the message.
pub async fn upload(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p):      Path<SourcePath>,
    mut multipart: Multipart,
) -> Result<Json<Vec<Attachment>>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    // Resolve (creating if needed) the source's session so uploads land in the
    // directory the message will reference.
    let session_id = ctx.chat_hub.session_handler(&p.source).await?.session_id;

    let dir_rel = format!("data/uploads/{}/{session_id}", auth.user_id);
    let dir_abs = fs_tools::resolve(&dir_rel)?;
    tokio::fs::create_dir_all(&dir_abs).await?;

    let mut saved: Vec<Attachment> = Vec::new();

    while let Some(mut field) = multipart.next_field().await
        .map_err(|e| ApiError::bad_request(format!("multipart error: {e}")))?
    {
        // Only fields carrying a filename are file uploads; skip plain text fields.
        let Some(orig_name) = field.file_name().map(str::to_string) else { continue };
        let mimetype = field.content_type().map(str::to_string);

        let base_name = sanitize_filename(&orig_name);
        let (abs_path, final_name) = unique_target(&dir_abs, &base_name);

        let mut file = tokio::fs::File::create(&abs_path).await
            .map_err(|e| ApiError::from(anyhow::anyhow!("cannot create {}: {e}", abs_path.display())))?;

        let mut size: u64 = 0;
        let mut too_large = false;
        while let Some(chunk) = field.chunk().await
            .map_err(|e| ApiError::bad_request(format!("upload read error: {e}")))?
        {
            size += chunk.len() as u64;
            if size > MAX_UPLOAD_BYTES {
                too_large = true;
                break;
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);

        if too_large {
            let _ = tokio::fs::remove_file(&abs_path).await;
            return Err(ApiError::payload_too_large(format!(
                "'{final_name}' exceeds the {} MiB upload limit",
                MAX_UPLOAD_BYTES / 1024 / 1024
            )));
        }

        // The sniffed type wins over the client claim when we recognize the bytes.
        let mimetype = sniff_head(&abs_path).await.map(String::from).or(mimetype);

        saved.push(Attachment {
            path:     format!("{dir_rel}/{final_name}"),
            name:     final_name,
            mimetype,
            filesize: Some(size),
        });
    }

    Ok(Json(saved))
}

/// Reads the first bytes of a saved upload and sniffs its real media type.
async fn sniff_head(path: &StdPath) -> Option<&'static str> {
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut head = [0u8; 16];
    let n = tokio::io::AsyncReadExt::read(&mut file, &mut head).await.ok()?;
    sniff_mime(&head[..n])
}

/// Reduces an arbitrary client filename to a safe basename: directory components
/// are dropped and an empty/`.`/`..` result falls back to `"file"`.
fn sanitize_filename(raw: &str) -> String {
    let base = StdPath::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .trim();
    if base.is_empty() || base == "." || base == ".." {
        "file".to_string()
    } else {
        base.to_string()
    }
}

/// Returns a non-colliding `(absolute_path, final_name)` inside `dir`. If `name`
/// already exists, inserts `_1`, `_2`, … before the extension.
fn unique_target(dir: &StdPath, name: &str) -> (PathBuf, String) {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return (candidate, name.to_string());
    }
    let path = StdPath::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext  = path.extension().and_then(|s| s.to_str());
    for n in 1.. {
        let next = match ext {
            Some(ext) => format!("{stem}_{n}.{ext}"),
            None      => format!("{stem}_{n}"),
        };
        let candidate = dir.join(&next);
        if !candidate.exists() {
            return (candidate, next);
        }
    }
    unreachable!("unique_target loop always returns")
}
