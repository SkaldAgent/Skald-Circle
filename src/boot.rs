//! Curated, human-readable bootstrap progress printed to **stdout**.
//!
//! At runtime stdout is silent — only the file log (`logs/skald.log`) records
//! events. During startup, though, it is useful to see at a glance how the app
//! is configured and how it is coming up. These helpers emit a small, ordered
//! set of lines on the dedicated `boot` tracing target, which a stdout layer
//! (see [`BootFormat`], wired in `main.rs`) renders cleanly — no timestamps,
//! levels or targets, just the message, with failures shown in red.
//!
//! The same lines also land in the log file (they pass the normal `EnvFilter`),
//! so they double as a high-level startup trace. Glyphs are baked into the
//! message on purpose, so the file keeps the same readable shape.
//!
//! The stdout layer filters on the `boot` target only and is independent of
//! `RUST_LOG`, so this output always appears regardless of the log filter.

use std::fmt;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, info, warn};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

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

/// Minimal stdout formatter for bootstrap lines: prints just the event's
/// message (no timestamp/level/target), in red when the level is WARN or ERROR.
pub struct BootFormat;

#[derive(Default)]
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write;
            let _ = write!(self.0, "{value:?}");
        }
    }
}

impl<S, N> FormatEvent<S, N> for BootFormat
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);

        // Level ordering in tracing: TRACE > DEBUG > INFO > WARN > ERROR, so
        // `<= WARN` matches both WARN and ERROR.
        let is_failure = *event.metadata().level() <= Level::WARN;
        if writer.has_ansi_escapes() && is_failure {
            write!(writer, "\u{1b}[31m{}\u{1b}[0m", visitor.0)?;
        } else {
            write!(writer, "{}", visitor.0)?;
        }
        writeln!(writer)
    }
}
