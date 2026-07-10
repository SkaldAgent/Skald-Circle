use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use axum::{
    Json,
    extract::{Multipart, Path, State},
};
use tokio::io::AsyncWriteExt;

use core_api::message_meta::Attachment;

use crate::core::skald::Skald;
use crate::core::tools::fs as fs_tools;
use super::ApiError;
use super::sessions::SourcePath;

/// `POST /api/{source}/uploads`
///
/// Accepts a `multipart/form-data` body with one or more file fields and saves
/// each under `data/uploads/{session_id}/`. Bytes are streamed straight to disk
/// (`field.chunk()` → file), never buffered whole in RAM, so arbitrarily large
/// files are fine — the route disables the default body-size limit (see router).
///
/// Returns the saved [`Attachment`]s (project-root-relative path, name, MIME,
/// size) so the client can show chips and echo them back when sending the message.
pub async fn upload(
    State(skald): State<Arc<Skald>>,
    Path(p):      Path<SourcePath>,
    mut multipart: Multipart,
) -> Result<Json<Vec<Attachment>>, ApiError> {
    // Resolve (creating if needed) the source's session so uploads land in the
    // directory the message will reference.
    let session_id = skald.chat_hub().session_handler(&p.source).await?.session_id;

    let dir_rel = format!("data/uploads/{session_id}");
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
        while let Some(chunk) = field.chunk().await
            .map_err(|e| ApiError::bad_request(format!("upload read error: {e}")))?
        {
            file.write_all(&chunk).await?;
            size += chunk.len() as u64;
        }
        file.flush().await?;

        saved.push(Attachment {
            path:     format!("{dir_rel}/{final_name}"),
            name:     final_name,
            mimetype,
            filesize: Some(size),
        });
    }

    Ok(Json(saved))
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
