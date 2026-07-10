//! Custom slash-command abstraction shared between the main crate and plugins.
//!
//! `LlmCommandManager` (main crate) implements [`CommandApi`]; plugins (e.g. the
//! Telegram bot) accept `Arc<dyn CommandApi>` and stay decoupled from the manager's
//! concrete type and from the file-system discovery logic. Commands live in
//! `commands/<name>/` and are read at request time (see `src/core/command/mod.rs`).
//!
//! This module is intentionally synchronous: command discovery is trivial file I/O
//! over tiny files with no DB/network dependency, so it does not need the
//! `async_trait` ceremony of the other plugin API traits.

use serde::{Deserialize, Serialize};

/// One enabled custom command — the listing DTO (no template body). Returned by
/// [`CommandApi::list_enabled`] for `/help` and the composer autocomplete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo {
    /// Canonical command name, without the leading `/` (e.g. `review`).
    pub name:        String,
    /// One-line description shown in `/help` and the autocomplete dropdown.
    pub description: String,
}

/// A resolved command ready for template expansion. Returned by
/// [`CommandApi::resolve`]. `name` is canonical (lowercase, no `/`); `template` is
/// the raw `COMMAND.md` body.
#[derive(Debug, Clone)]
pub struct ResolvedCommand {
    pub name:     String,
    pub template: String,
}

/// Expand a command template by substituting the user's arguments. Replaces
/// `{{args}}` / `{{prompt}}` with `args`; if neither placeholder is present, the
/// arguments are appended after a `---` separator (opencode-like default). An empty
/// `args` with no placeholder leaves the template verbatim.
pub fn expand_template(template: &str, args: &str) -> String {
    if template.contains("{{args}}") || template.contains("{{prompt}}") {
        template.replace("{{args}}", args).replace("{{prompt}}", args)
    } else if args.is_empty() {
        template.to_string()
    } else {
        format!("{template}\n\n---\n\n{args}")
    }
}

/// Abstraction over the command manager that plugins depend on. The main crate's
/// `LlmCommandManager` is the only implementation.
pub trait CommandApi: Send + Sync {
    /// Every enabled, non-reserved command (metadata only — no template body),
    /// sorted by name. Tolerant of a missing `commands/` directory (returns empty).
    fn list_enabled(&self) -> Vec<CommandInfo>;

    /// Resolve a command by name (case-insensitive), loading its template body.
    /// Returns `None` when the command does not exist, is disabled, or is reserved
    /// (collides with a hard-coded system command).
    fn resolve(&self, name: &str) -> Option<ResolvedCommand>;
}
