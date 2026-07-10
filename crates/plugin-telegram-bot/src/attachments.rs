use std::path::Path;

use anyhow::Result;
use core_api::message_meta::Attachment;
use teloxide::net::Download;
use teloxide::prelude::*;

/// A media item sent by the user via Telegram.
///
/// # Extending
/// Add a new variant here, then handle it in:
///   1. `handlers::classify_message`  — detect the message type and build the variant
///   2. `TelegramAttachment::download_and_save` — fetch bytes and persist to disk,
///                                                 returning an [`Attachment`]
///                                                 (return `Ok(None)` if no file is involved)
///   3. `TelegramAttachment::system_info_message` — describe a file-less variant
///                                                   (Location) for the LLM
pub(crate) enum TelegramAttachment {
    Document {
        file_id:   String,
        file_name: String,
        mime_type: Option<String>,
        caption:   Option<String>,
    },
    Photo {
        file_id: String,
        caption: Option<String>,
    },
    Location {
        latitude:  f64,
        longitude: f64,
        accuracy:  Option<f64>,
        /// True when the user shared a live location (continuously updated).
        is_live: bool,
    },
}

impl TelegramAttachment {
    /// Downloads the attachment from Telegram, writes it to `base_dir/<chat_id>/<name>`,
    /// and returns the saved [`Attachment`] (shared with the web/mobile path so the
    /// copilot UI renders it identically). Returns `None` for attachment types that
    /// carry no binary content (e.g. Location).
    ///
    /// The returned `path` is made relative to the process working directory (the
    /// project root) when possible, so it is both servable under `/data/…` and
    /// resolvable by the filesystem tools — matching web uploads.
    pub(crate) async fn download_and_save(
        &self,
        bot:      &Bot,
        base_dir: &Path,
        chat_id:  i64,
    ) -> Result<Option<Attachment>> {
        let (file_id, file_name, mimetype): (&str, String, Option<String>) = match self {
            Self::Document { file_id, file_name, mime_type, .. } =>
                (file_id, file_name.clone(), mime_type.clone()),
            Self::Photo    { file_id, .. } =>
                (file_id, format!("{file_id}.jpg"), Some("image/jpeg".to_string())),
            Self::Location { .. } => return Ok(None),
        };

        let dir = base_dir.join(chat_id.to_string());
        tokio::fs::create_dir_all(&dir).await?;

        let tg_file = bot.get_file(teloxide::types::FileId(file_id.to_string())).await?;
        let mut bytes = Vec::new();
        bot.download_file(&tg_file.path, &mut bytes).await?;

        let path = dir.join(&file_name);
        tokio::fs::write(&path, &bytes).await?;

        // Prefer a project-root-relative path so `/data/…` serving works.
        let rel = std::env::current_dir()
            .ok()
            .and_then(|cwd| path.strip_prefix(&cwd).ok().map(Path::to_path_buf))
            .unwrap_or_else(|| path.clone());

        Ok(Some(Attachment {
            path:     rel.to_string_lossy().to_string(),
            name:     file_name,
            mimetype,
            filesize: Some(bytes.len() as u64),
        }))
    }

    /// Builds the `[TELEGRAM SYSTEM INFO]` message injected into the conversation history.
    /// `saved_path` is `None` for attachment types that produce no file on disk.
    pub(crate) fn system_info_message(&self, saved_path: Option<&Path>) -> String {
        match self {
            Self::Document { file_name, mime_type, caption, .. } => {
                let mime = mime_type.as_deref().unwrap_or("application/octet-stream");
                let path = saved_path.map(|p| p.display().to_string()).unwrap_or_default();
                format!(
                    "[TELEGRAM SYSTEM INFO]\n\
                     The user has sent a file attachment.\n\
                     File name: {file_name}\n\
                     MIME type: {mime}\n\
                     Saved at:  {path}{}",
                    caption_line(caption.as_deref()),
                )
            }
            Self::Photo { caption, .. } => {
                let path = saved_path.map(|p| p.display().to_string()).unwrap_or_default();
                format!(
                    "[TELEGRAM SYSTEM INFO]\n\
                     The user has sent a photo.\n\
                     Saved at: {path}{}",
                    caption_line(caption.as_deref()),
                )
            }
            Self::Location { latitude, longitude, accuracy, is_live } => {
                let maps_url = format!("https://maps.google.com/?q={latitude},{longitude}");
                let accuracy_line = accuracy
                    .map(|a| format!("\nAccuracy: ±{a:.0} m"))
                    .unwrap_or_default();
                let kind = if *is_live { "live location (snapshot at time of receipt)" } else { "location" };
                format!(
                    "[TELEGRAM SYSTEM INFO]\n\
                     The user has shared a {kind}.\n\
                     Latitude:  {latitude}\n\
                     Longitude: {longitude}{accuracy_line}\n\
                     Maps URL:  {maps_url}"
                )
            }
        }
    }
}

fn caption_line(caption: Option<&str>) -> String {
    caption
        .map(|c| format!("\nCaption: {c}"))
        .unwrap_or_default()
}
