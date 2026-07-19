//! How this binary renders the bootstrap lines that `skald_core::boot` emits.
//!
//! Rendering is the shell's business, not the core's: another shell (e.g. the
//! setup wizard) formats the same `boot` target differently, or not at all.
//! Wired in `main.rs` as a stdout layer filtered on `boot::TARGET`, independent
//! of `RUST_LOG`, so this output always appears whatever the log filter is.

use std::fmt;

use tracing::field::{Field, Visit};
use tracing::{Event, Level};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::{FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

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
