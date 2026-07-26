//! `SkaldMediaSource` — **which** files may reach a model
//! (`agent_loop::projection::MediaSource`).
//!
//! The split with the crate is the §6 containment boundary: the library decides
//! shape, capability and budget; this decides *authorization*, and only files
//! that pass are ever handed over as blobs.
//!
//! Two paths, two rules:
//!
//! - **uploaded attachments** must resolve, through the caller's [`UserFs`],
//!   under their `~/uploads/` — where the upload seam writes them. An image
//!   sitting anywhere else in the workspace is never inlined just because a
//!   message mentions it.
//! - **tool-produced media** must land under one of the caller's workspace
//!   roots (home, shared folders, projects, docs). The tool already resolved
//!   and contained the path, so this is a fail-closed re-check against a
//!   symlink swapped since the read.
//!
//! Both are re-checked here even though the paths came from trusted code: the
//! container is writable by the agent, so any host-side read must re-verify.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_loop::projection::{MediaBlob, MediaSource};
use agent_loop::store::{StoredCall, StoredMessage};
use core_api::message_meta::{Attachment, MessageMetadata, attachments_block};
use core_api::tool::MediaRef;
use core_api::user_fs::{UPLOADS_SUBDIR, UserFs};
use tracing::debug;

/// A contained file, read lazily.
struct FileBlob {
    name: String,
    /// `None` = failed authorization; every read then returns `None`, so the
    /// projection skips it (fail-closed, no panic, no partial inline).
    path: Option<PathBuf>,
}

#[agent_loop::async_trait]
impl MediaBlob for FileBlob {
    fn name(&self) -> &str {
        &self.name
    }

    async fn size(&self) -> Option<u64> {
        let path = self.path.as_ref()?;
        tokio::fs::metadata(path).await.ok().map(|m| m.len())
    }

    async fn head(&self) -> Option<Vec<u8>> {
        let path = self.path.as_ref()?;
        let mut file = tokio::fs::File::open(path).await.ok()?;
        let mut head = [0u8; 16];
        let n = tokio::io::AsyncReadExt::read(&mut file, &mut head).await.ok()?;
        Some(head[..n].to_vec())
    }

    async fn read_all(&self) -> Option<Vec<u8>> {
        let path = self.path.as_ref()?;
        tokio::fs::read(path).await.ok()
    }
}

/// The uploads directory, canonicalized for prefix-checking.
fn uploads_root(fs: &UserFs) -> Option<PathBuf> {
    std::fs::canonicalize(fs.home_host.join(UPLOADS_SUBDIR)).ok()
}

/// The caller's workspace roots: private home, each shared folder, each project,
/// and the read-only docs mount.
fn workspace_roots(fs: &UserFs) -> Vec<PathBuf> {
    let canon =
        |p: &Path| crate::tools::fs::canonicalize_for_policy(&p.to_string_lossy(), Path::new("/"));
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

/// One blob per attachment, **in attachment order** — an unauthorized one
/// yields a blob that reads as nothing, so positions stay aligned with the
/// caller's list and the projection simply skips it.
pub fn attachment_blobs(fs: &UserFs, attachments: &[Attachment]) -> Vec<Arc<dyn MediaBlob>> {
    let root = uploads_root(fs);
    attachments
        .iter()
        .map(|a| {
            let path = root.as_ref().and_then(|root| {
                let abs = crate::tools::fs::resolve_host_path(fs, &a.path).ok()?;
                if abs.starts_with(root) {
                    Some(abs)
                } else {
                    debug!(path = %a.path, "media not inlined: outside the uploads root");
                    None
                }
            });
            Arc::new(FileBlob { name: a.name.clone(), path }) as Arc<dyn MediaBlob>
        })
        .collect()
}

/// Blobs for tool-produced media, dropping anything outside the workspace.
pub fn ref_blobs(fs: &UserFs, refs: &[MediaRef]) -> Vec<Arc<dyn MediaBlob>> {
    let roots = workspace_roots(fs);
    refs.iter()
        .filter_map(|r| {
            let canon = crate::tools::fs::canonicalize_for_policy(&r.host_path, Path::new("/"));
            if !roots.iter().any(|root| crate::tools::fs::path_under(&canon, root)) {
                debug!(path = %r.host_path, "tool media not inlined: outside the workspace");
                return None;
            }
            let name = canon
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            Some(Arc::new(FileBlob { name, path: Some(canon) }) as Arc<dyn MediaBlob>)
        })
        .collect()
}

/// The caller's media authorization.
pub struct SkaldMediaSource {
    fs: Arc<UserFs>,
}

impl SkaldMediaSource {
    pub fn new(fs: Arc<UserFs>) -> Self {
        Self { fs }
    }

    /// The attachments a stored message carries, in wire order.
    fn attachments(msg: &StoredMessage) -> Vec<Attachment> {
        msg.metadata
            .as_ref()
            .and_then(|v| serde_json::from_value::<MessageMetadata>(v.clone()).ok())
            .map(|m| m.attachments)
            .unwrap_or_default()
    }

}

#[agent_loop::async_trait]
impl MediaSource for SkaldMediaSource {
    async fn message_media(&self, msg: &StoredMessage) -> Vec<Arc<dyn MediaBlob>> {
        // Positions matter: `skipped_text` indexes this same list.
        attachment_blobs(&self.fs, &Self::attachments(msg))
    }

    async fn call_media(&self, calls: &[StoredCall]) -> Vec<Arc<dyn MediaBlob>> {
        // Tool media rides `extras.media` as a JSON string of `MediaRef`s.
        let refs: Vec<MediaRef> = calls
            .iter()
            .filter_map(|c| c.extras["media"].as_str())
            .filter_map(|s| serde_json::from_str::<Vec<MediaRef>>(s).ok())
            .flatten()
            .collect();
        ref_blobs(&self.fs, &refs)
    }

    fn skipped_text(&self, msg: &StoredMessage, skipped: &[usize]) -> Option<String> {
        if skipped.is_empty() {
            return None;
        }
        let attachments = Self::attachments(msg);
        let left: Vec<Attachment> = skipped
            .iter()
            .filter_map(|&i| attachments.get(i).cloned())
            .collect();
        if left.is_empty() {
            return None;
        }
        // The textual path block: the agent can still read these with a tool.
        Some(attachments_block(&left))
    }
}

#[cfg(test)]
mod tests {
    //! What may be inlined — the §6 half. The library's budgets and part shapes
    //! are tested in `agent_loop::projection::media`; these assert the
    //! authorization: uploads only, workspace only, fail-closed on traversal.

    use super::*;
    use agent_loop::projection::{MediaBudget, media::partition};

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

    fn pdf_bytes() -> Vec<u8> {
        let mut v = b"%PDF-1.7\n".to_vec();
        v.extend_from_slice(&[0x00; 64]);
        v
    }

    fn caps(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    /// A throwaway [`UserFs`] whose private home is `root/homes/u1`.
    fn fs_home(home: &Path) -> UserFs {
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

    /// `(inlined parts, skipped positions)` for a message's attachments.
    async fn inline(
        attachments: &[Attachment],
        capabilities: &[String],
        fs: &UserFs,
    ) -> (Vec<serde_json::Value>, Vec<usize>) {
        let blobs = attachment_blobs(fs, attachments);
        partition(&blobs, capabilities, &MediaBudget::default()).await
    }

    #[tokio::test]
    async fn an_uploaded_png_reaches_a_vision_model() {
        let tmp = std::env::temp_dir().join(format!("skald-media-{}", uuid::Uuid::new_v4()));
        let home = tmp.join("homes/u1");
        let dir = home.join("uploads/1");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.png"), png_bytes()).await.unwrap();
        let fs = fs_home(&home);

        let (parts, skipped) = inline(&[att("uploads/1/a.png")], &caps(&["vision"]), &fs).await;
        assert!(skipped.is_empty());
        assert_eq!(parts.len(), 1);
        assert!(
            parts[0]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn only_the_uploads_directory_is_authorized() {
        let tmp = std::env::temp_dir().join(format!("skald-media-{}", uuid::Uuid::new_v4()));
        let home = tmp.join("homes/u1");
        let dir = home.join("uploads/1");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.png"), png_bytes()).await.unwrap();
        // A real image inside the home but OUTSIDE the uploads dir.
        tokio::fs::write(home.join("secret.png"), png_bytes()).await.unwrap();
        let fs = fs_home(&home);

        // No capability → everything stays textual.
        let (parts, skipped) = inline(&[att("uploads/1/a.png")], &caps(&[]), &fs).await;
        assert_eq!(skipped.len(), 1);
        assert!(parts.is_empty());

        // An image elsewhere in the home is never inlined…
        let (parts, skipped) = inline(&[att("secret.png")], &caps(&["vision"]), &fs).await;
        assert_eq!(skipped.len(), 1);
        assert!(parts.is_empty());

        // …and traversal out of the workspace is rejected fail-closed.
        let (parts, skipped) =
            inline(&[att("uploads/../../secret.png")], &caps(&["vision"]), &fs).await;
        assert_eq!(skipped.len(), 1);
        assert!(parts.is_empty());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn a_pdf_needs_the_document_capability() {
        let tmp = std::env::temp_dir().join(format!("skald-media-{}", uuid::Uuid::new_v4()));
        let home = tmp.join("homes/u1");
        let dir = home.join("uploads/1");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("a.pdf"), pdf_bytes()).await.unwrap();
        let fs = fs_home(&home);

        let (parts, _) = inline(&[att("uploads/1/a.pdf")], &caps(&["document"]), &fs).await;
        assert_eq!(parts[0]["type"], "file");
        assert_eq!(parts[0]["file"]["filename"], "a.pdf");

        // vision alone does not unlock PDFs.
        let (_, skipped) = inline(&[att("uploads/1/a.pdf")], &caps(&["vision"]), &fs).await;
        assert_eq!(skipped.len(), 1);

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[tokio::test]
    async fn tool_media_is_contained_to_the_workspace() {
        let tmp = std::env::temp_dir().join(format!("skald-toolmedia-{}", uuid::Uuid::new_v4()));
        let home = tmp.join("homes/u1");
        tokio::fs::create_dir_all(&home).await.unwrap();
        tokio::fs::write(home.join("pic.png"), png_bytes()).await.unwrap();
        tokio::fs::write(tmp.join("outside.png"), png_bytes()).await.unwrap();
        let fs = fs_home(&home);

        let inside = MediaRef {
            host_path: home.join("pic.png").to_string_lossy().into_owned(),
            mime:      "image/png".into(),
        };
        let outside = MediaRef {
            host_path: tmp.join("outside.png").to_string_lossy().into_owned(),
            mime:      "image/png".into(),
        };
        let refs = |r: &MediaRef| ref_blobs(&fs, std::slice::from_ref(r));

        let (parts, _) =
            partition(&refs(&inside), &caps(&["vision"]), &MediaBudget::default()).await;
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "image_url");

        // No capability → nothing inlined.
        let (parts, _) = partition(&refs(&inside), &caps(&[]), &MediaBudget::default()).await;
        assert!(parts.is_empty());
        // A real image outside the workspace never becomes a blob at all.
        assert!(ref_blobs(&fs, std::slice::from_ref(&outside)).is_empty());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }
}
