use axum::{
    extract::{Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use tokio::fs;

use std::sync::Arc;
use crate::core::mcp::content_type_for_ext;
use crate::core::skald::Skald;

/// GET /api/mcp-media/:file
///
/// Serves a persisted MCP tool-result media file from `data/mcp_media/<file>`.
/// `file` is a flat `<id>.<ext>` name produced by `McpManager::persist_media`;
/// the `Content-Type` is inferred from the extension.
pub async fn get_media(
    State(skald): State<Arc<Skald>>,
    Path(file): Path<String>,
) -> Response {
    // Reject path traversal: only a flat `<id>.<ext>` filename is allowed.
    if file.contains('/') || file.contains('\\') || file.contains("..") {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let ext  = file.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    let path = skald.mcp().media_dir().join(&file);

    match fs::read(&path).await {
        Ok(bytes) => {
            let mut response = bytes.into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type_for_ext(ext)),
            );
            response
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}
