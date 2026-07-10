//! Curated, human-readable bootstrap progress printed to **stdout**.
//!
//! At runtime stdout is silent — only the file log (`logs/skald.log`) records
//! events. During startup, though, it is useful to see at a glance how the app
//! is configured and how it is coming up. These helpers emit a small, ordered
//! set of lines on the dedicated `boot` tracing target.
//!
//! Rendering belongs to whoever owns the process: each shell installs a stdout
//! layer filtered on [`TARGET`] and formats the lines as it likes (`skald`'s
//! `BootFormat` strips timestamps and paints failures red). The core only says
//! *what* happened, never how it looks — which is also why nothing here depends
//! on `tracing-subscriber`.
//!
//! The same lines land in the log file (they pass the normal `EnvFilter`), so
//! they double as a high-level startup trace. Glyphs are baked into the message
//! on purpose, so the file keeps the same readable shape.

use std::fmt;

use tracing::{info, warn};

/// Tracing target for curated bootstrap lines shown on stdout.
pub const TARGET: &str = "boot";

/// Top-level title (no glyph), e.g. `skald v0.5 — starting`.
pub fn title(msg: impl fmt::Display) {
    info!(target: TARGET, "{}", msg);
}

/// A phase header, e.g. `› Plugins — 6 active, 1 failed`.
pub fn section(msg: impl fmt::Display) {
    info!(target: TARGET, "› {}", msg);
}

/// A successful item under a phase.
pub fn ok(msg: impl fmt::Display) {
    info!(target: TARGET, "  ✓ {}", msg);
}

/// An item that exists but is inactive (e.g. a disabled plugin).
pub fn off(msg: impl fmt::Display) {
    info!(target: TARGET, "  ○ {}", msg);
}

/// A failed item (rendered in red on stdout; logged at WARN in the file).
pub fn fail(msg: impl fmt::Display) {
    warn!(target: TARGET, "  ✗ {}", msg);
}

/// The final "app is up" line, e.g. `✅ Ready — http://localhost:8080`.
pub fn ready(msg: impl fmt::Display) {
    info!(target: TARGET, "✅ {}", msg);
}
