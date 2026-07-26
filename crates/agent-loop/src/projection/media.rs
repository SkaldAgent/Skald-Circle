//! The wire half of multimodal media: which files a model can take, in which
//! content-part shape, within which budgets.
//!
//! The host supplies **blobs** it has already authorized (containment, upload
//! rules, ownership — its policy); this module decides whether a blob reaches
//! the model and in what shape. The split is deliberate: the part shapes and
//! the byte ceilings are protocol (`MAX_DOCUMENT_BYTES` is literally
//! Anthropic's per-request document ceiling), the authorization is not.
//!
//! Promotion is strict: a blob is inlined only when the model declares the
//! modality's capability, the **sniffed magic bytes** match an allowed MIME (a
//! host-claimed MIME is never trusted — there is no seam to pass one), and the
//! per-file / per-turn budgets hold. Anything failing a check is reported back
//! as skipped so the host can keep it on its textual path.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use serde_json::{Value, json};
use tracing::debug;

/// Max media parts inlined per turn.
pub const MAX_MEDIA_PER_TURN: usize = 4;
/// Max bytes for one inlined image.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
/// Max bytes for one inlined video.
pub const MAX_VIDEO_BYTES: u64 = 32 * 1024 * 1024;
/// Max bytes for one inlined document (Anthropic's per-request ceiling).
pub const MAX_DOCUMENT_BYTES: u64 = 32 * 1024 * 1024;
/// Max combined media bytes inlined per turn.
pub const MAX_TOTAL_MEDIA_BYTES: u64 = 48 * 1024 * 1024;

// ── MediaKind ────────────────────────────────────────────────────────────────

/// A model-input modality: the capability that unlocks it and the content-part
/// shape it maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    /// PDFs, as the OpenAI file-input part (`{"type":"file","file":{…}}`) —
    /// forwarded verbatim by OpenAI-compatible clients and translated to a
    /// native `document` block by the Anthropic client.
    Document,
}

impl MediaKind {
    /// The `ModelInfo::capabilities` entry that unlocks this modality.
    pub fn capability(self) -> &'static str {
        match self {
            Self::Image    => "vision",
            Self::Video    => "video",
            Self::Document => "document",
        }
    }

    /// The OpenAI content-part type.
    pub fn part_type(self) -> &'static str {
        match self {
            Self::Image    => "image_url",
            Self::Video    => "video_url",
            Self::Document => "file",
        }
    }

    /// Human-readable format list (hosts use it in tool descriptions).
    pub fn formats(self) -> &'static str {
        match self {
            Self::Image    => "images (PNG, JPEG, GIF, WebP)",
            Self::Video    => "video (MP4, WebM, MOV, …)",
            Self::Document => "PDF documents",
        }
    }

    /// The modality a sniffed MIME belongs to.
    pub fn for_mime(mime: &str) -> Option<Self> {
        match mime {
            "image/png" | "image/jpeg" | "image/gif" | "image/webp" => Some(Self::Image),
            "video/mp4" | "video/mpeg" | "video/quicktime" | "video/webm" | "video/x-msvideo"
            | "video/x-flv" | "video/3gpp" => Some(Self::Video),
            "application/pdf" => Some(Self::Document),
            _ => None,
        }
    }

    /// The modalities a model with these capabilities can take, in a stable order.
    pub fn enabled(capabilities: &[String]) -> Vec<Self> {
        [Self::Image, Self::Video, Self::Document]
            .into_iter()
            .filter(|k| capabilities.iter().any(|c| c == k.capability()))
            .collect()
    }
}

// ── MediaBudget ──────────────────────────────────────────────────────────────

/// Per-file and per-turn ceilings.
#[derive(Debug, Clone, Copy)]
pub struct MediaBudget {
    pub max_per_turn:        usize,
    pub max_image_bytes:     u64,
    pub max_video_bytes:     u64,
    pub max_document_bytes:  u64,
    pub max_total_bytes:     u64,
}

impl Default for MediaBudget {
    fn default() -> Self {
        Self {
            max_per_turn:       MAX_MEDIA_PER_TURN,
            max_image_bytes:    MAX_IMAGE_BYTES,
            max_video_bytes:    MAX_VIDEO_BYTES,
            max_document_bytes: MAX_DOCUMENT_BYTES,
            max_total_bytes:    MAX_TOTAL_MEDIA_BYTES,
        }
    }
}

impl MediaBudget {
    pub fn max_bytes(&self, kind: MediaKind) -> u64 {
        match kind {
            MediaKind::Image    => self.max_image_bytes,
            MediaKind::Video    => self.max_video_bytes,
            MediaKind::Document => self.max_document_bytes,
        }
    }
}

// ── MediaBlob ────────────────────────────────────────────────────────────────

/// A candidate medium the host has already authorized. Reads are lazy so a
/// blob rejected on capability or size is never fully loaded.
#[async_trait]
pub trait MediaBlob: Send + Sync {
    /// Display name (the `filename` of a `file` part).
    fn name(&self) -> &str;
    /// Byte length; `None` (unknown) means "do not inline".
    async fn size(&self) -> Option<u64>;
    /// The first bytes, for magic-byte sniffing (16 are enough).
    async fn head(&self) -> Option<Vec<u8>>;
    /// The whole content.
    async fn read_all(&self) -> Option<Vec<u8>>;
}

// ── projection ───────────────────────────────────────────────────────────────

/// The OpenAI-wire content part for one inlined medium.
pub fn media_part(kind: MediaKind, mime: &str, bytes: &[u8], filename: &str) -> Value {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let url = format!("data:{mime};base64,{b64}");
    match kind {
        MediaKind::Document => {
            json!({ "type": "file", "file": { "filename": filename, "file_data": url } })
        }
        k => {
            let t = k.part_type();
            json!({ "type": t, t: { "url": url } })
        }
    }
}

/// Splits blobs into inline content parts and the indices left out.
///
/// Skipped blobs are the host's business: it typically renders them as a
/// textual path list so the agent can still read them with a tool.
pub async fn partition(
    blobs:        &[Arc<dyn MediaBlob>],
    capabilities: &[String],
    budget:       &MediaBudget,
) -> (Vec<Value>, Vec<usize>) {
    if blobs.is_empty() {
        return (Vec::new(), Vec::new());
    }
    if MediaKind::enabled(capabilities).is_empty() {
        return (Vec::new(), (0..blobs.len()).collect());
    }

    let mut parts: Vec<Value> = Vec::new();
    let mut skipped: Vec<usize> = Vec::new();
    let mut total: u64 = 0;

    for (idx, blob) in blobs.iter().enumerate() {
        if parts.len() >= budget.max_per_turn {
            debug!(name = blob.name(), "media not inlined: per-turn count budget exhausted");
            skipped.push(idx);
            continue;
        }
        match promote(blob.as_ref(), capabilities, budget, total).await {
            Some((part, bytes)) => {
                total += bytes;
                parts.push(part);
            }
            None => skipped.push(idx),
        }
    }
    (parts, skipped)
}

/// Sniff + capability + budget + build, for one blob. `None` (logged at debug)
/// when it is not a recognized medium, the model lacks the modality, or a byte
/// budget is exhausted. The per-turn **count** budget is the caller's.
async fn promote(
    blob:         &dyn MediaBlob,
    capabilities: &[String],
    budget:       &MediaBudget,
    used_total:   u64,
) -> Option<(Value, u64)> {
    let head = blob.head().await?;
    let mime = sniff_mime(&head)?;
    let kind = MediaKind::for_mime(mime)?;
    if !capabilities.iter().any(|c| c == kind.capability()) {
        debug!(name = blob.name(), mime, "media not inlined: model lacks the capability");
        return None;
    }

    let size = blob.size().await?;
    if size > budget.max_bytes(kind) {
        debug!(name = blob.name(), size, "media not inlined: file too large");
        return None;
    }
    if used_total + size > budget.max_total_bytes {
        debug!(name = blob.name(), "media not inlined: per-turn byte budget exhausted");
        return None;
    }

    let bytes = blob.read_all().await?;
    Some((media_part(kind, mime, &bytes, blob.name()), size))
}

/// Sniffs the magic bytes of a medium we know how to inline, returning its
/// canonical MIME type. `None` = not a recognized medium (not an error —
/// ordinary files simply are not model input).
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
    if head.starts_with(b"%PDF-") {
        return Some("application/pdf");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory blob.
    struct Blob {
        name:  String,
        bytes: Vec<u8>,
    }

    /// A blob as the trait object the engine takes.
    fn blob(name: &str, bytes: Vec<u8>) -> Arc<dyn MediaBlob> {
        Arc::new(Blob { name: name.to_string(), bytes })
    }

    #[async_trait]
    impl MediaBlob for Blob {
        fn name(&self) -> &str { &self.name }
        async fn size(&self) -> Option<u64> { Some(self.bytes.len() as u64) }
        async fn head(&self) -> Option<Vec<u8>> {
            Some(self.bytes.iter().copied().take(16).collect())
        }
        async fn read_all(&self) -> Option<Vec<u8>> { Some(self.bytes.clone()) }
    }

    fn png() -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.extend_from_slice(&[0xAA; 64]);
        v
    }

    fn pdf() -> Vec<u8> {
        let mut v = b"%PDF-1.7\n".to_vec();
        v.extend_from_slice(&[0x00; 64]);
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
        assert_eq!(sniff_mime(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(sniff_mime(b""), None);
    }

    #[tokio::test]
    async fn inlines_png_for_a_vision_model() {
        let (parts, skipped) =
            partition(&[blob("a.png", png())], &caps(&["vision"]), &MediaBudget::default()).await;
        assert!(skipped.is_empty());
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "image_url");
        assert!(
            parts[0]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,")
        );
    }

    #[tokio::test]
    async fn inlines_pdf_as_a_file_part_for_a_document_model() {
        let (parts, skipped) =
            partition(&[blob("a.pdf", pdf())], &caps(&["document"]), &MediaBudget::default()).await;
        assert!(skipped.is_empty());
        assert_eq!(parts[0]["type"], "file");
        assert_eq!(parts[0]["file"]["filename"], "a.pdf");
        assert!(
            parts[0]["file"]["file_data"].as_str().unwrap().starts_with("data:application/pdf;base64,")
        );
    }

    #[tokio::test]
    async fn gates_on_capability_per_modality() {
        let b = |bytes: Vec<u8>| vec![blob("x", bytes)];
        let budget = MediaBudget::default();

        // No capability at all.
        let (parts, skipped) = partition(&b(png()), &caps(&[]), &budget).await;
        assert!(parts.is_empty() && skipped == vec![0]);

        // vision does not unlock PDFs, document does not unlock images.
        let (parts, skipped) = partition(&b(pdf()), &caps(&["vision"]), &budget).await;
        assert!(parts.is_empty() && skipped == vec![0]);
        let (parts, skipped) = partition(&b(png()), &caps(&["document"]), &budget).await;
        assert!(parts.is_empty() && skipped == vec![0]);

        // An unrecognized medium is never inlined.
        let (parts, skipped) = partition(&b(b"plain text".to_vec()), &caps(&["vision"]), &budget).await;
        assert!(parts.is_empty() && skipped == vec![0]);
    }

    #[tokio::test]
    async fn enforces_count_per_file_and_total_budgets() {
        let budget = MediaBudget::default();
        let blobs: Vec<Arc<dyn MediaBlob>> = (0..budget.max_per_turn + 2)
            .map(|i| blob(&format!("{i}.png"), png()))
            .collect();
        let (parts, skipped) = partition(&blobs, &caps(&["vision"]), &budget).await;
        assert_eq!(parts.len(), budget.max_per_turn);
        assert_eq!(skipped.len(), 2);

        // Per-file ceiling.
        let tight = MediaBudget { max_image_bytes: 8, ..MediaBudget::default() };
        let (parts, skipped) = partition(&[blob("a.png", png())], &caps(&["vision"]), &tight).await;
        assert!(parts.is_empty() && skipped == vec![0]);

        // Per-turn total: the first fits, the second does not.
        let total = MediaBudget { max_total_bytes: 100, ..MediaBudget::default() };
        let (parts, skipped) = partition(
            &[blob("a.png", png()), blob("b.png", png())],
            &caps(&["vision"]),
            &total,
        )
        .await;
        assert_eq!(parts.len(), 1);
        assert_eq!(skipped, vec![1]);
    }

    #[test]
    fn enabled_modalities_are_capability_driven() {
        assert!(MediaKind::enabled(&caps(&[])).is_empty());
        assert_eq!(MediaKind::enabled(&caps(&["vision"])), vec![MediaKind::Image]);
        assert_eq!(
            MediaKind::enabled(&caps(&["document", "vision"])),
            vec![MediaKind::Image, MediaKind::Document],
            "the order is the enum's, not the capability list's"
        );
    }
}
