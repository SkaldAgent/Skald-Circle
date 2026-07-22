//! Centralized upload persistence.
//!
//! The single place any surface — the web `POST /uploads` handler, a channel
//! plugin like Telegram, or a future one — turns received file bytes into a
//! saved [`Attachment`]. Keeping placement + naming + metadata here means no two
//! callers can drift on *where* an upload lands or *what* path is recorded (the
//! bug this fixes: uploads that the agent then couldn't reach).
//!
//! Files are written into the user's private container home under
//! `uploads/{session_id}/…`. That single agent path is resolved identically by
//! every consumer — the fs-tools, `execute_cmd` (the home is bind-mounted at
//! `/root`), the file viewer (`/api/file`) and the media inliner — through the
//! same per-user [`UserFs`].

use std::path::{Path, PathBuf};

use core_api::message_meta::Attachment;
use core_api::user_fs::{UserFs, UPLOADS_SUBDIR};

use crate::session::handler::media::sniff_mime;

/// Persist `bytes` as a file named `file_name` into the user's
/// `~/uploads/{session_id}/`, returning the resulting [`Attachment`] whose `path`
/// is the home-relative agent path. The recognized magic-byte MIME wins over the
/// caller-claimed `client_mime`.
///
/// The caller owns byte transport (streaming a multipart body, downloading from
/// an API) and any size cap; this owns placement, collision-safe naming, MIME
/// sniffing and the metadata shape.
pub async fn save_to_home(
    fs: &UserFs,
    session_id: i64,
    file_name: &str,
    client_mime: Option<String>,
    bytes: &[u8],
) -> std::io::Result<Attachment> {
    let dir_host = fs
        .home_host
        .join(UPLOADS_SUBDIR)
        .join(session_id.to_string());
    tokio::fs::create_dir_all(&dir_host).await?;

    let (abs_path, final_name) = unique_target(&dir_host, &sanitize_filename(file_name));
    tokio::fs::write(&abs_path, bytes).await?;

    // The sniffed type wins over the client claim when we recognize the bytes.
    let mimetype = sniff_mime(&bytes[..bytes.len().min(16)])
        .map(str::to_string)
        .or(client_mime);

    Ok(Attachment {
        path:     format!("{UPLOADS_SUBDIR}/{session_id}/{final_name}"),
        name:     final_name,
        mimetype,
        filesize: Some(bytes.len() as u64),
    })
}

/// Reduces an arbitrary client filename to a safe basename: directory components
/// are dropped and an empty/`.`/`..` result falls back to `"file"`.
fn sanitize_filename(raw: &str) -> String {
    let base = Path::new(raw)
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
fn unique_target(dir: &Path, name: &str) -> (PathBuf, String) {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return (candidate, name.to_string());
    }
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = path.extension().and_then(|s| s.to_str());
    for n in 1.. {
        let next = match ext {
            Some(ext) => format!("{stem}_{n}.{ext}"),
            None => format!("{stem}_{n}"),
        };
        let candidate = dir.join(&next);
        if !candidate.exists() {
            return (candidate, next);
        }
    }
    unreachable!("unique_target loop always returns")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway [`UserFs`] whose private home is `home`.
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

    fn pdf_bytes() -> Vec<u8> {
        let mut v = b"%PDF-1.7\n".to_vec();
        v.extend_from_slice(&[0x00; 32]);
        v
    }

    #[tokio::test]
    async fn saves_into_home_uploads_with_agent_path_and_sniffs_mime() {
        let tmp = std::env::temp_dir().join(format!("skald-upload-{}", uuid::Uuid::new_v4()));
        let home = tmp.join("homes/u1");
        tokio::fs::create_dir_all(&home).await.unwrap();
        let fs = fs_home(&home);

        // A wrong client MIME is overridden by the sniffed PDF signature.
        let att = save_to_home(&fs, 7, "cv.pdf", Some("application/octet-stream".into()), &pdf_bytes())
            .await
            .unwrap();

        assert_eq!(att.path, "uploads/7/cv.pdf");
        assert_eq!(att.name, "cv.pdf");
        assert_eq!(att.mimetype.as_deref(), Some("application/pdf"));
        assert_eq!(att.filesize, Some(pdf_bytes().len() as u64));
        // Physically lands under the home's uploads dir (reachable by the agent).
        assert!(home.join("uploads/7/cv.pdf").exists());

        // A second upload of the same name never overwrites — it is de-duped.
        let att2 = save_to_home(&fs, 7, "cv.pdf", None, &pdf_bytes()).await.unwrap();
        assert_eq!(att2.path, "uploads/7/cv_1.pdf");
        assert!(home.join("uploads/7/cv_1.pdf").exists());

        let _ = tokio::fs::remove_dir_all(&tmp).await;
    }

    #[test]
    fn sanitize_strips_directory_components() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("a/b/c.txt"), "c.txt");
        assert_eq!(sanitize_filename(".."), "file");
        assert_eq!(sanitize_filename(""), "file");
    }
}
