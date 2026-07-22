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

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{json, Value};
use tracing::debug;

use core_api::message_meta::Attachment;
use core_api::tool::MediaRef;
use core_api::user_fs::UserFs;

/// Max media parts inlined per turn.
const MAX_MEDIA_PER_TURN: usize = 4;
/// Max bytes for one inlined image.
const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
/// Max bytes for one inlined video.
const MAX_VIDEO_BYTES: u64 = 32 * 1024 * 1024;
/// Max bytes for one inlined PDF (Anthropic's per-request document ceiling).
const MAX_PDF_BYTES: u64 = 32 * 1024 * 1024;
/// Max combined media bytes inlined per turn.
const MAX_TOTAL_MEDIA_BYTES: u64 = 48 * 1024 * 1024;

/// A model-input modality: the capability that unlocks it, the content-part
/// type it maps to, its byte cap, the sniffed MIME types accepted, and a
/// human-readable format list for the `read_file` description.
struct Modality {
    capability: &'static str,
    part_type:  &'static str,
    max_bytes:  u64,
    mimes:      &'static [&'static str],
    formats:    &'static str,
}

const MODALITIES: &[Modality] = &[
    Modality {
        capability: "vision",
        part_type:  "image_url",
        max_bytes:  MAX_IMAGE_BYTES,
        mimes:      &["image/png", "image/jpeg", "image/gif", "image/webp"],
        formats:    "images (PNG, JPEG, GIF, WebP)",
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
        formats:    "video (MP4, WebM, MOV, …)",
    },
    // PDF documents. The `file` part is the OpenAI file-input shape
    // (`{"type":"file","file":{"filename","file_data"}}`), forwarded verbatim by
    // OpenAI-compatible clients and translated to a native `document` block by the
    // Anthropic client. Gated on the `document` capability, so a model row without
    // it (any OpenAI-compat endpoint that can't take a `file` part) never receives
    // one — set the capability only on rows whose endpoint accepts PDFs.
    Modality {
        capability: "document",
        part_type:  "file",
        max_bytes:  MAX_PDF_BYTES,
        mimes:      &["application/pdf"],
        formats:    "PDF documents",
    },
];

/// Builds the OpenAI-wire content part for one inlined medium. Images/video use the
/// `{"type":"image_url"|"video_url","…":{"url":data-URL}}` shape; PDFs use the
/// `file` shape carrying a filename + `file_data` data-URL.
fn build_media_part(part_type: &str, mime: &str, b64: &str, filename: &str) -> Value {
    let url = format!("data:{mime};base64,{b64}");
    match part_type {
        "file" => json!({ "type": "file", "file": { "filename": filename, "file_data": url } }),
        t      => json!({ "type": t, t: { "url": url } }),
    }
}

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

/// Promotes one uploaded attachment to a content part, or `None` when any check
/// fails (logged at debug level; the caller keeps it on the textual path).
/// Containment is against the uploads `root`; the rest is [`promote`].
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
    promote(&abs, &a.name, capabilities, used_total).await
}

/// Read + sniff + capability/budget check + build the content part for one file at
/// an **already-contained** absolute path. Shared by the uploaded-attachment path
/// ([`try_inline`]) and the tool-produced-media path ([`inline_paths`]); neither
/// containment nor per-turn count budget is enforced here — the callers do that.
/// `None` (logged at debug) when the file is not a recognized medium, the model
/// lacks the modality, or a byte budget is exhausted.
async fn promote(
    abs: &Path,
    filename: &str,
    capabilities: &[String],
    used_total: u64,
) -> Option<(Value, u64)> {
    let mut file = tokio::fs::File::open(abs).await.ok()?;
    let mut head = [0u8; 16];
    let n = tokio::io::AsyncReadExt::read(&mut file, &mut head).await.ok()?;
    let mime = sniff_mime(&head[..n])?;
    let modality = MODALITIES.iter().find(|m| m.mimes.contains(&mime))?;
    if !capabilities.iter().any(|c| c == modality.capability) {
        debug!(path = %abs.display(), mime, "media not inlined: model lacks the capability");
        return None;
    }

    let size = file.metadata().await.ok()?.len();
    if size > modality.max_bytes {
        debug!(path = %abs.display(), size, "media not inlined: file too large");
        return None;
    }
    if used_total + size > MAX_TOTAL_MEDIA_BYTES {
        debug!(path = %abs.display(), "media not inlined: per-turn byte budget exhausted");
        return None;
    }

    let bytes = tokio::fs::read(abs).await.ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some((build_media_part(modality.part_type, mime, &b64, filename), size))
}

/// Inline media a tool produced (e.g. `read_file` on an image) as content parts,
/// for the current turn only. Mirrors [`partition_under`] but contains against the
/// caller's **workspace roots** (home + shared + projects + docs) rather than the
/// uploads dir — the tool already resolved + contained the path, so this is a
/// fail-closed re-check against a symlink swap since the read (§6). Same per-file,
/// per-count and per-turn byte budgets; the capability gate lives here, so a
/// tool always records the media and the model only sees it when able.
pub async fn inline_paths(
    refs:         &[MediaRef],
    capabilities: &[String],
    fs:           &UserFs,
) -> Vec<Value> {
    let capable = MODALITIES
        .iter()
        .any(|m| capabilities.iter().any(|c| c == m.capability));
    if !capable || refs.is_empty() {
        return Vec::new();
    }
    let roots = workspace_roots(fs);
    if roots.is_empty() {
        return Vec::new();
    }

    let mut parts: Vec<Value> = Vec::new();
    let mut total: u64 = 0;
    for r in refs {
        if parts.len() >= MAX_MEDIA_PER_TURN {
            break;
        }
        let canon = crate::tools::fs::canonicalize_for_policy(&r.host_path, Path::new("/"));
        if !roots.iter().any(|root| crate::tools::fs::path_under(&canon, root)) {
            debug!(path = %r.host_path, "tool media not inlined: outside the workspace");
            continue;
        }
        let filename = canon
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".to_string());
        if let Some((part, bytes)) = promote(&canon, &filename, capabilities, total).await {
            total += bytes;
            parts.push(part);
        }
    }
    parts
}

/// The caller's workspace roots, canonicalized for prefix-checking: private home,
/// each shared folder, each project, and the read-only docs mount.
fn workspace_roots(fs: &UserFs) -> Vec<PathBuf> {
    let canon = |p: &Path| crate::tools::fs::canonicalize_for_policy(&p.to_string_lossy(), Path::new("/"));
    let mut roots = vec![canon(&fs.home_host)];
    for m in &fs.shared {
        roots.push(canon(&m.host));
    }
    for m in &fs.projects {
        roots.push(canon(&m.host));
    }
    if let Some(d) = &fs.docs_host {
        roots.push(canon(d));
    }
    roots
}

/// Sentence appended to `read_file`'s description when the resolved model can view
/// media, naming the formats it takes as native input. `None` when the model has
/// no media modality (description stays unchanged). See `call_llm_round`.
pub fn media_capability_hint(capabilities: &[String]) -> Option<String> {
    let forms: Vec<&'static str> = MODALITIES
        .iter()
        .filter(|m| capabilities.iter().any(|c| c == m.capability))
        .map(|m| m.formats)
        .collect();
    if forms.is_empty() {
        return None;
    }
    Some(format!(
        " This model can view {} directly: when you read_file one of these, its content is given to you as native model input (not text).",
        join_human(&forms),
    ))
}

/// `["a"] → "a"`, `["a","b"] → "a and b"`, `["a","b","c"] → "a, b, and c"`.
fn join_human(items: &[&str]) -> String {
    match items {
        []                => String::new(),
        [a]               => a.to_string(),
        [a, b]            => format!("{a} and {b}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// Opens a file and sniffs its first bytes, returning a recognized media MIME
/// (`image/*`, `video/*`, `application/pdf`) or `None` for an ordinary/unreadable
/// file. Used by `read_file` to decide whether to hand a file back as native media
/// rather than trying to read it as UTF-8 text.
pub async fn probe_media(path: &Path) -> Option<&'static str> {
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut head = [0u8; 16];
    let n = tokio::io::AsyncReadExt::read(&mut file, &mut head).await.ok()?;
    sniff_mime(&head[..n])
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
    if head.starts_with(b"%PDF-") {
        return Some("application/pdf");
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
        assert_eq!(sniff_mime(b"%PDF-1.7"), Some("application/pdf"));
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

    fn pdf_bytes() -> Vec<u8> {
        let mut v = b"%PDF-1.7\n".to_vec();
        v.extend_from_slice(&[0x00; 64]);
        v
    }

    #[tokio::test]
    async fn partition_inlines_pdf_as_file_part_for_document_model() {
        let tmp = std::env::temp_dir().join(format!("skald-media-{}", uuid::Uuid::new_v4()));
        let dir = tmp.join("data/uploads/u/1");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.pdf"), pdf_bytes()).await.unwrap();

        // A document-capable model inlines the PDF as the OpenAI `file` part shape.
        let p = partition_under(&[att("data/uploads/u/1/a.pdf")], &caps(&["document"]), &tmp).await;
        assert!(p.rest.is_empty());
        assert_eq!(p.parts.len(), 1);
        assert_eq!(p.parts[0]["type"], "file");
        assert_eq!(p.parts[0]["file"]["filename"], "a.pdf");
        let fd = p.parts[0]["file"]["file_data"].as_str().unwrap();
        assert!(fd.starts_with("data:application/pdf;base64,"), "{fd}");

        // vision alone does not unlock PDFs.
        let p = partition_under(&[att("data/uploads/u/1/a.pdf")], &caps(&["vision"]), &tmp).await;
        assert_eq!(p.rest.len(), 1);
        assert!(p.parts.is_empty());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    /// A throwaway [`UserFs`] whose private home is `root/homes/u1`.
    fn fs_home(home: &std::path::Path) -> UserFs {
        UserFs::new(
            "u1",
            home.to_path_buf(),
            "skald-u1",
            PathBuf::from("/root"),
            vec![],
            vec![],
            None,
        )
    }

    #[tokio::test]
    async fn inline_paths_contains_and_gates_on_capability() {
        let tmp = std::env::temp_dir().join(format!("skald-toolmedia-{}", uuid::Uuid::new_v4()));
        let home = tmp.join("homes/u1");
        tokio::fs::create_dir_all(&home).await.unwrap();
        tokio::fs::write(home.join("pic.png"), png_bytes()).await.unwrap();
        tokio::fs::write(tmp.join("outside.png"), png_bytes()).await.unwrap();
        let fs = fs_home(&home);

        let inside = MediaRef { host_path: home.join("pic.png").to_string_lossy().into_owned(), mime: "image/png".into() };
        let outside = MediaRef { host_path: tmp.join("outside.png").to_string_lossy().into_owned(), mime: "image/png".into() };

        // capable + inside the home → one image part.
        let parts = inline_paths(std::slice::from_ref(&inside), &caps(&["vision"]), &fs).await;
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "image_url");
        assert!(parts[0]["image_url"]["url"].as_str().unwrap().starts_with("data:image/png;base64,"));

        // no capability → nothing inlined.
        assert!(inline_paths(std::slice::from_ref(&inside), &caps(&[]), &fs).await.is_empty());

        // a real image outside the workspace is rejected fail-closed.
        assert!(inline_paths(std::slice::from_ref(&outside), &caps(&["vision"]), &fs).await.is_empty());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[test]
    fn media_capability_hint_lists_enabled_formats_only() {
        assert!(media_capability_hint(&caps(&[])).is_none());
        let h = media_capability_hint(&caps(&["vision"])).unwrap();
        assert!(h.contains("images (PNG, JPEG, GIF, WebP)"), "{h}");
        assert!(!h.contains("PDF"), "{h}");
        let h = media_capability_hint(&caps(&["vision", "document"])).unwrap();
        assert!(h.contains("images (PNG, JPEG, GIF, WebP)") && h.contains("PDF documents"), "{h}");
    }
}
