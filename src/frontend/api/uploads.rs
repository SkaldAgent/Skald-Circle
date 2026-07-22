use std::sync::Arc;

use axum::{
    Json, Extension,
    extract::{Multipart, Path, State},
};

use core_api::message_meta::Attachment;

use skald_core::skald::Skald;
use super::{ApiError, guard::AuthUser, require_context};
use super::sessions::SourcePath;

/// Max bytes accepted for a single uploaded file; anything larger is refused 413.
const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// `POST /api/{source}/uploads`
///
/// Accepts a `multipart/form-data` body with one or more file fields and persists
/// each through the shared upload seam ([`skald_core::chat_hub::ChatHub::save_upload`]),
/// which saves into the caller's container home under `uploads/{session_id}/` —
/// so a single agent path (`uploads/{session_id}/…`) is reachable by the fs-tools,
/// by `execute_cmd` (the home is bind-mounted at `/root`), and by the file viewer
/// alike. The web handler and every channel plugin go through that one seam, so no
/// two surfaces can drift on *where* an upload lands.
///
/// Each field is read with the [`MAX_UPLOAD_BYTES`] cap enforced during accumulation
/// (an over-cap field is refused before anything is written); the route disables the
/// default body-size limit (see router). The seam sniffs the magic bytes and prefers
/// them over the client-supplied `Content-Type`.
///
/// Returns the saved [`Attachment`]s (home-relative agent path, name, MIME, size) so
/// the client can show chips and echo them back when sending the message.
pub async fn upload(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(p):      Path<SourcePath>,
    mut multipart: Multipart,
) -> Result<Json<Vec<Attachment>>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;

    let mut saved: Vec<Attachment> = Vec::new();

    while let Some(mut field) = multipart.next_field().await
        .map_err(|e| ApiError::bad_request(format!("multipart error: {e}")))?
    {
        // Only fields carrying a filename are file uploads; skip plain text fields.
        let Some(orig_name) = field.file_name().map(str::to_string) else { continue };
        let client_mime = field.content_type().map(str::to_string);

        // Buffer the field, enforcing the size cap as we read so an over-limit
        // upload is refused before any bytes are handed to the store.
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = field.chunk().await
            .map_err(|e| ApiError::bad_request(format!("upload read error: {e}")))?
        {
            if bytes.len() as u64 + chunk.len() as u64 > MAX_UPLOAD_BYTES {
                return Err(ApiError::payload_too_large(format!(
                    "'{orig_name}' exceeds the {} MiB upload limit",
                    MAX_UPLOAD_BYTES / 1024 / 1024
                )));
            }
            bytes.extend_from_slice(&chunk);
        }

        let att = ctx.chat_hub.save_upload(&p.source, &orig_name, client_mime, &bytes).await?;
        saved.push(att);
    }

    Ok(Json(saved))
}
