//! Inline multimodal media for chat attachments.
//!
//! Attachments normally reach the model as a textual list of paths (see
//! `attachments_block`) and the agent decides whether to read them. When the
//! resolved model declares a matching capability (`vision`, `video`), media
//! attachments of the **current turn** are instead sent as native content
//! parts — `image_url` / `video_url` data URLs, the OpenAI wire shape, which
//! non-OpenAI clients translate — so the model actually sees the bytes.
//!
//! Promotion is deliberately strict: an attachment is inlined only when ALL of
//! these hold —
//! - the model has the modality's capability;
//! - the file lives under `data/uploads/`, canonicalized (attachments saved
//!   anywhere else, e.g. by the Telegram plugin, stay textual);
//! - the sniffed magic bytes match an allowed MIME — the client-supplied
//!   `mimetype` is never trusted;
//! - the per-file and per-turn byte/count budgets are not exhausted.
//!
//! Anything failing a check silently stays on the textual path.

use std::path::Path;

use base64::Engine as _;
use serde_json::{json, Value};
use tracing::debug;

use core_api::message_meta::Attachment;

/// Max media parts inlined per turn.
const MAX_MEDIA_PER_TURN: usize = 4;
/// Max bytes for one inlined image.
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
/// Max bytes for one inlined video.
const MAX_VIDEO_BYTES: u64 = 32 * 1024 * 1024;
/// Max combined media bytes inlined per turn.
const MAX_TOTAL_MEDIA_BYTES: u64 = 48 * 1024 * 1024;

/// A model-input modality: the capability that unlocks it, the content-part
/// type it maps to, its byte cap and the sniffed MIME types accepted.
struct Modality {
    capability: &'static str,
    part_type:  &'static str,
    max_bytes:  u64,
    mimes:      &'static [&'static str],
}

const MODALITIES: &[Modality] = &[
    Modality {
        capability: "vision",
        part_type:  "image_url",
        max_bytes:  MAX_IMAGE_BYTES,
        mimes:      &["image/png", "image/jpeg", "image/gif", "image/webp"],
    },
    Modality {
        capability: "video",
        part_type:  "video_url",
        max_bytes:  MAX_VIDEO_BYTES,
        mimes:      &[
            "video/mp4",
            "video/mpeg",
            "video/quicktime",
            "video/webm",
            "video/x-msvideo",
            "video/x-flv",
            "video/3gpp",
        ],
    },
];

/// The result of partitioning a message's attachments.
pub struct MediaPartition {
    /// OpenAI-style content parts, ready to append after the text part.
    pub parts: Vec<Value>,
    /// Attachments that stay on the textual path block.
    pub rest:  Vec<Attachment>,
}

/// Splits a message's attachments into inline media parts and leftovers.
/// Files are resolved against the process working directory.
pub async fn partition(attachments: &[Attachment], capabilities: &[String]) -> MediaPartition {
    let base = std::env::current_dir().unwrap_or_default();
    partition_under(attachments, capabilities, &base).await
}

/// [`partition`] with an explicit base directory (tests).
pub async fn partition_under(
    attachments: &[Attachment],
    capabilities: &[String],
    base: &Path,
) -> MediaPartition {
    let capable = MODALITIES
        .iter()
        .any(|m| capabilities.iter().any(|c| c == m.capability));
    let root = std::fs::canonicalize(base.join("data").join("uploads")).ok();
    if !capable || root.is_none() {
        return MediaPartition { parts: Vec::new(), rest: attachments.to_vec() };
    }
    let root = root.unwrap();

    let mut parts: Vec<Value> = Vec::new();
    let mut rest: Vec<Attachment> = Vec::new();
    let mut total: u64 = 0;
    for a in attachments {
        if parts.len() >= MAX_MEDIA_PER_TURN {
            debug!(path = %a.path, "media not inlined: per-turn count budget exhausted");
            rest.push(a.clone());
            continue;
        }
        match try_inline(a, capabilities, base, &root, total).await {
            Some((part, bytes)) => {
                total += bytes;
                parts.push(part);
            }
            None => rest.push(a.clone()),
        }
    }
    MediaPartition { parts, rest }
}

/// Promotes one attachment to a content part, or `None` when any check fails
/// (logged at debug level; the caller keeps it on the textual path).
async fn try_inline(
    a: &Attachment,
    capabilities: &[String],
    base: &Path,
    root: &Path,
    used_total: u64,
) -> Option<(Value, u64)> {
    let abs = tokio::fs::canonicalize(base.join(&a.path)).await.ok()?;
    if !abs.starts_with(root) {
        debug!(path = %a.path, "media not inlined: outside the uploads root");
        return None;
    }

    let mut file = tokio::fs::File::open(&abs).await.ok()?;
    let mut head = [0u8; 16];
    let n = tokio::io::AsyncReadExt::read(&mut file, &mut head).await.ok()?;
    let mime = sniff_mime(&head[..n])?;
    let modality = MODALITIES.iter().find(|m| m.mimes.contains(&mime))?;
    if !capabilities.iter().any(|c| c == modality.capability) {
        debug!(path = %a.path, mime, "media not inlined: model lacks the capability");
        return None;
    }

    let size = file.metadata().await.ok()?.len();
    if size > modality.max_bytes {
        debug!(path = %a.path, size, "media not inlined: file too large");
        return None;
    }
    if used_total + size > MAX_TOTAL_MEDIA_BYTES {
        debug!(path = %a.path, "media not inlined: per-turn byte budget exhausted");
        return None;
    }

    let bytes = tokio::fs::read(&abs).await.ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let url = format!("data:{mime};base64,{b64}");
    let t = modality.part_type;
    Some((json!({ "type": t, t: { "url": url } }), size))
}

/// Sniffs the magic bytes of a medium we know how to inline, returning its
/// canonical MIME type. `None` = not a recognized medium (not an error —
/// ordinary files simply stay on the textual path).
pub fn sniff_mime(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if head.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if head.len() >= 12 && &head[4..8] == b"ftyp" {
        let brand = &head[8..12];
        if brand.starts_with(b"3gp") || brand.starts_with(b"3g2") {
            return Some("video/3gpp");
        }
        if brand == b"qt  " {
            return Some("video/quicktime");
        }
        // isom / mp41 / mp42 / avc1 / M4V …
        return Some("video/mp4");
    }
    // EBML header — WebM (and Matroska, close enough for the video models).
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some("video/webm");
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"AVI " {
        return Some("video/x-msvideo");
    }
    if head.starts_with(b"FLV\x01") {
        return Some("video/x-flv");
    }
    if head.starts_with(&[0x00, 0x00, 0x01, 0xBA]) || head.starts_with(&[0x00, 0x00, 0x01, 0xB3]) {
        return Some("video/mpeg");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(path: &str) -> Attachment {
        Attachment {
            path:     path.to_string(),
            name:     path.rsplit('/').next().unwrap().to_string(),
            mimetype: None,
            filesize: None,
        }
    }

    fn png_bytes() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0xAA; 64]);
        v
    }

    fn caps(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sniff_known_signatures() {
        assert_eq!(sniff_mime(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff_mime(b"\xff\xd8\xff\xe0...."), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_mime(b"RIFF\x00\x00\x00\x00WEBP"), Some("image/webp"));
        assert_eq!(sniff_mime(b"\x00\x00\x00\x18ftypisom"), Some("video/mp4"));
        assert_eq!(sniff_mime(b"\x00\x00\x00\x18ftypqt  "), Some("video/quicktime"));
        assert_eq!(sniff_mime(b"\x00\x00\x00\x18ftyp3gp4"), Some("video/3gpp"));
        assert_eq!(sniff_mime(&[0x1A, 0x45, 0xDF, 0xA3, 0, 0]), Some("video/webm"));
        assert_eq!(sniff_mime(b"RIFF\x00\x00\x00\x00AVI "), Some("video/x-msvideo"));
        assert_eq!(sniff_mime(b"FLV\x01\x05"), Some("video/x-flv"));
        assert_eq!(sniff_mime(&[0x00, 0x00, 0x01, 0xBA]), Some("video/mpeg"));
        assert_eq!(sniff_mime(b"%PDF-1.7"), None);
        assert_eq!(sniff_mime(b""), None);
    }

    #[tokio::test]
    async fn partition_inlines_png_for_vision_model() {
        let tmp = std::env::temp_dir().join(format!("skald-media-{}", uuid::Uuid::new_v4()));
        let dir = tmp.join("data/uploads/u/1");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.png"), png_bytes()).await.unwrap();

        let p = partition_under(&[att("data/uploads/u/1/a.png")], &caps(&["vision"]), &tmp).await;
        assert!(p.rest.is_empty());
        assert_eq!(p.parts.len(), 1);
        let url = p.parts[0]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"));

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn partition_gates_on_capability_and_containment() {
        let tmp = std::env::temp_dir().join(format!("skald-media-{}", uuid::Uuid::new_v4()));
        let dir = tmp.join("data/uploads/u/1");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.png"), png_bytes()).await.unwrap();
        tokio::fs::write(tmp.join("secret.png"), png_bytes()).await.unwrap();

        // No capability → everything stays textual.
        let p = partition_under(&[att("data/uploads/u/1/a.png")], &caps(&[]), &tmp).await;
        assert_eq!(p.rest.len(), 1);
        assert!(p.parts.is_empty());

        // vision capability does not unlock video parts.
        let p = partition_under(&[att("data/uploads/u/1/a.png")], &caps(&["video"]), &tmp).await;
        assert_eq!(p.rest.len(), 1);

        // A real image outside the uploads root is never read inline.
        let p = partition_under(&[att("secret.png")], &caps(&["vision"]), &tmp).await;
        assert_eq!(p.rest.len(), 1);
        assert!(p.parts.is_empty());

        // Traversal out of the root is rejected.
        let p = partition_under(&[att("data/uploads/../../secret.png")], &caps(&["vision"]), &tmp).await;
        assert_eq!(p.rest.len(), 1);
        assert!(p.parts.is_empty());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn partition_enforces_count_budget() {
        let tmp = std::env::temp_dir().join(format!("skald-media-{}", uuid::Uuid::new_v4()));
        let dir = tmp.join("data/uploads/u/1");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let mut atts = Vec::new();
        for i in 0..(MAX_MEDIA_PER_TURN + 2) {
            let rel = format!("data/uploads/u/1/{i}.png");
            tokio::fs::write(dir.join(format!("{i}.png")), png_bytes()).await.unwrap();
            atts.push(att(&rel));
        }
        let p = partition_under(&atts, &caps(&["vision"]), &tmp).await;
        assert_eq!(p.parts.len(), MAX_MEDIA_PER_TURN);
        assert_eq!(p.rest.len(), 2);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
