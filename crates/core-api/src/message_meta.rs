//! Structured, reusable metadata attached to a `chat_history` row.
//!
//! Persisted as a single JSON column (`chat_history.metadata`) and intentionally
//! generic: today it carries user file **attachments**, but new keys can be added
//! later without a schema change. Two independent readers derive different views
//! from the same source:
//!   - the **LLM context** builder appends [`attachments_block`] to the user turn,
//!   - the **history UI** renders the structured attachments as chips.
//!
//! The raw `[SYSTEM INFO]` text block is therefore never persisted — it is
//! generated on the fly from this metadata.

use serde::{Deserialize, Serialize};

/// One file attached by the user to a message. `path` is relative to the project
/// root (e.g. `data/uploads/123/file.pdf`) so it is both servable under `/data/…`
/// and resolvable by the filesystem tools.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub path:     String,
    pub name:     String,
    /// Best-effort MIME type (e.g. `application/pdf`); `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mimetype: Option<String>,
    /// Size in bytes, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesize: Option<u64>,
}

/// Generic metadata bag for a chat message. Extra keys may be added over time;
/// `#[serde(default)]` keeps deserialization tolerant of older/newer shapes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Present when this user turn was produced by a custom slash command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandRef>,
}

impl MessageMetadata {
    /// True when there is nothing worth persisting.
    pub fn is_empty(&self) -> bool {
        self.attachments.is_empty() && self.command.is_none()
    }
}

/// Identifies a user message produced by a custom slash command. The history row's
/// `content` holds the **expanded template** (replayed to the LLM verbatim); the UI
/// renders `display` — the original `/command …` the user typed — instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandRef {
    /// Canonical command name, without the leading `/` (e.g. `review`).
    pub name:    String,
    /// The original text the user typed (e.g. `/review revisiona Cat.java`).
    pub display: String,
}

/// Renders the human-readable block appended to a user turn so the LLM learns
/// which files were attached. Returns an empty string when there are none, so
/// callers can unconditionally concatenate it.
///
/// Shared by the web/mobile path and the Telegram plugin so every surface emits
/// an identical format.
pub fn attachments_block(attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let noun = if attachments.len() == 1 { "file" } else { "files" };
    let mut block = format!(
        "\n\n[SYSTEM INFO]\n{} attached {}:",
        attachments.len(),
        noun
    );
    for a in attachments {
        block.push_str(&format!("\n* {}", a.path));
    }
    block
}
