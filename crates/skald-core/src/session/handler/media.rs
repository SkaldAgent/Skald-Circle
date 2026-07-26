//! Media helpers that are Skald's, not the protocol's.
//!
//! The wire half — which modality a model can take, the content-part shapes,
//! the data-URL encoding, the byte budgets, the magic-byte sniffing — lives in
//! `agent_loop::projection::media`. What is left here is the app's own:
//!
//! - [`probe_media`] / [`media_capability_hint`]: what `read_file` tells the
//!   agent it can hand back as native model input.
//!
//! Everything that decides WHICH files may be inlined is
//! `loop_adapters::media_source::SkaldMediaSource` (§6 containment), and the
//! projection itself is the library's — neither lives here.

use std::path::Path;

use agent_loop::projection::media::MediaKind;

pub use agent_loop::projection::media::sniff_mime;

/// Sentence appended to `read_file`'s description when the resolved model can
/// view media, naming the formats it takes as native input. `None` when the
/// model has no media modality (the description stays unchanged).
pub fn media_capability_hint(capabilities: &[String]) -> Option<String> {
    let forms: Vec<&'static str> =
        MediaKind::enabled(capabilities).into_iter().map(|k| k.formats()).collect();
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

/// Opens a file and sniffs its first bytes, returning a recognized media MIME or
/// `None` for an ordinary/unreadable file. Used by `read_file` to decide whether
/// to hand a file back as native media rather than reading it as UTF-8 text.
pub async fn probe_media(path: &Path) -> Option<&'static str> {
    let mut file = tokio::fs::File::open(path).await.ok()?;
    let mut head = [0u8; 16];
    let n = tokio::io::AsyncReadExt::read(&mut file, &mut head).await.ok()?;
    sniff_mime(&head[..n])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
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

    #[tokio::test]
    async fn probe_media_recognizes_a_png_and_ignores_text() {
        let dir = std::env::temp_dir().join(format!("skald-probe-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let png = dir.join("a.png");
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0xAA; 32]);
        tokio::fs::write(&png, bytes).await.unwrap();
        let txt = dir.join("a.txt");
        tokio::fs::write(&txt, b"hello").await.unwrap();

        assert_eq!(probe_media(&png).await, Some("image/png"));
        assert_eq!(probe_media(&txt).await, None);
        assert_eq!(probe_media(&dir.join("missing")).await, None);

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
