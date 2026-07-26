//! Structured, reusable metadata attached to a `chat_history` row.
//!
//! Persisted as a single JSON column (`chat_history.metadata`) and intentionally
//! generic: today it carries user file **attachments**, but new keys can be added
//! later without a schema change. Two independent readers derive different views
//! from the same source:
//!   - the **LLM context** builder appends [`attachments_block`] to the user turn,
//!   - the **history UI** renders the structured attachments as chips.
//!
//! The raw `<system-extra>` text block is therefore never persisted — it is
//! generated on the fly from this metadata. The tag name lives in
//! [`SYSTEM_EXTRA_TAG`] so emission sites and the agent-facing instruction that
//! documents it can never drift apart.

use serde::{Deserialize, Serialize};

/// One file attached by the user to a message. `path` is a home-relative agent
/// path (e.g. `uploads/123/file.pdf`) — the caller's container home is its root,
/// so the fs-tools, `execute_cmd`, the file viewer (`/api/file`) and the media
/// inliner all resolve it to the same physical file.
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

/// The canonical name of the tag that wraps harness-injected data (attachments,
/// locations, transcripts, hook output…) inside user messages and tool results.
///
/// Single source of truth: every emission site builds via [`system_extra`], and
/// the agent-facing instruction that documents the tag interpolates this same
/// constant (via the `__HARNESS_TAG__` substitution). Renaming the tag is a
/// one-line change here.
pub const SYSTEM_EXTRA_TAG: &str = "system-extra";

/// Wraps a harness-generated body in the canonical `<system-extra>` block, with
/// a leading blank-line pair so it can be concatenated onto the tail of a user
/// message or a tool result. Returns the full block (open tag, body, close tag).
///
/// Callers must not add their own leading newlines — this helper owns the
/// framing. An empty `body` still emits the (empty) block; callers that want a
/// no-op on empty input should check themselves (as [`attachments_block`] does).
pub fn system_extra(body: &str) -> String {
    format!("\n\n<{TAG}>\n{body}\n</{TAG}>", TAG = SYSTEM_EXTRA_TAG)
}

/// Renders the human-readable block appended to a user turn so the LLM learns
/// which files were attached. Returns an empty string when there are none, so
/// callers can unconditionally concatenate it.
///
/// Shared by the web/mobile path and the Telegram plugin so every surface emits
/// an identical format. The wrapping tag is [`SYSTEM_EXTRA_TAG`].
pub fn attachments_block(attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return String::new();
    }
    let noun = if attachments.len() == 1 { "file" } else { "files" };
    let mut body = format!("{} attached {}:", attachments.len(), noun);
    for a in attachments {
        body.push_str(&format!("\n* {}", a.path));
    }
    system_extra(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_extra_wraps_body_in_tag() {
        let out = system_extra("hello");
        let open = format!("<{TAG}>", TAG = SYSTEM_EXTRA_TAG);
        let close = format!("</{TAG}>", TAG = SYSTEM_EXTRA_TAG);
        assert!(out.starts_with("\n\n"), "leading blank-line pair: {:?}", out);
        assert!(out.contains(&open), "open tag missing: {:?}", out);
        assert!(out.contains(&close), "close tag missing: {:?}", out);
        assert_eq!(out, "\n\n<system-extra>\nhello\n</system-extra>");
    }

    #[test]
    fn system_extra_tag_name_follows_constant() {
        // If this breaks, emission and the documented name have diverged: rename
        // via SYSTEM_EXTRA_TAG only, never by editing this string.
        assert_eq!(SYSTEM_EXTRA_TAG, "system-extra");
        let out = system_extra("x");
        let tag = SYSTEM_EXTRA_TAG;
        assert!(out.contains(&format!("<{tag}>")) && out.contains(&format!("</{tag}>")));
    }

    #[test]
    fn attachments_block_empty_is_empty() {
        assert_eq!(attachments_block(&[]), "");
    }

    #[test]
    fn attachments_block_lists_paths_inside_tag() {
        let a = Attachment {
            path:     "uploads/1/a.png".into(),
            name:     "a.png".into(),
            mimetype: None,
            filesize: None,
        };
        let b = Attachment {
            path:     "uploads/1/b.pdf".into(),
            name:     "b.pdf".into(),
            mimetype: None,
            filesize: None,
        };
        let out = attachments_block(&[a, b]);
        // Pluralised noun, both paths, wrapped in the canonical tag.
        assert!(out.contains("2 attached files:"));
        assert!(out.contains("* uploads/1/a.png"));
        assert!(out.contains("* uploads/1/b.pdf"));
        assert!(out.contains(&format!("<{TAG}>", TAG = SYSTEM_EXTRA_TAG)));
        assert!(out.contains(&format!("</{TAG}>", TAG = SYSTEM_EXTRA_TAG)));
    }
}
