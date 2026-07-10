//! File-based custom slash commands.
//!
//! Each command lives in `commands/<name>/` with a `meta.json` manifest and a
//! `COMMAND.md` template — mirroring the `agents/<id>/` layout. Files are read at
//! request time so edits take effect without a restart (like `agents::load_prompt`).
//!
//! A recognised `/command` expands its `COMMAND.md` template (interpolating the
//! user's arguments into `{{args}}` / `{{prompt}}`) into a **normal user message on
//! the `main` session**, so the turn stays fully interactive: the model can ask
//! questions, iterate, and dispatch sub-agents exactly as in any other turn.
//!
//! [`CommandApi`] (in `core-api`) is the capability trait plugins depend on; this
//! manager is its only implementation.

use core_api::command::{CommandApi, CommandInfo, ResolvedCommand, expand_template};
use serde::Deserialize;
use tracing::warn;

const COMMANDS_DIR: &str = "commands";

/// Command names that collide with the hard-coded system commands in the WS handler.
/// System commands are matched first, so a same-named custom command is unreachable;
/// these are also filtered out of discovery/listing to avoid dead entries in `/help`
/// and the autocomplete.
const RESERVED: &[&str] = &[
    "clear", "new", "help", "context", "cost", "compact",
    "resettools", "models", "model", "sethome", "stop",
];

/// The `meta.json` manifest of a custom command.
#[derive(Debug, Clone, Deserialize)]
struct RawMeta {
    description: String,
    #[serde(default = "default_true")]
    enabled:     bool,
}

fn default_true() -> bool { true }

/// File-based manager for custom slash commands. Owned by `Skald` (wrapped in
/// `Arc` so it can be shared with plugins as `Arc<dyn CommandApi>`); stateless — it
/// re-reads `commands/` on each call, so edits take effect without a restart.
pub struct LlmCommandManager;

impl LlmCommandManager {
    pub fn new() -> Self { Self }

    /// True when `name` collides with a hard-coded system command.
    pub fn is_reserved(name: &str) -> bool {
        RESERVED.contains(&name.to_ascii_lowercase().as_str())
    }

    /// Expand a command template by substituting the user's arguments.
    /// Thin wrapper over [`core_api::command::expand_template`] so callers that hold
    /// the concrete manager (e.g. the WS handler) keep their existing call sites.
    pub fn expand(&self, template: &str, args: &str) -> String {
        expand_template(template, args)
    }
}

impl Default for LlmCommandManager {
    fn default() -> Self { Self::new() }
}

impl CommandApi for LlmCommandManager {
    /// Every enabled, non-reserved command (metadata only — no template body),
    /// sorted by name. Tolerant of a missing `commands/` directory (returns empty).
    fn list_enabled(&self) -> Vec<CommandInfo> {
        let mut out = Vec::new();
        let dir = match std::fs::read_dir(COMMANDS_DIR) {
            Ok(d)  => d,
            Err(_) => return out, // no commands/ dir yet → no custom commands
        };
        for entry in dir.flatten() {
            let path = entry.path();
            if !path.is_dir() { continue; }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) if !n.is_empty() => n.to_string(),
                _ => continue,
            };
            if Self::is_reserved(&name) { continue; }
            if !path.join("meta.json").exists() || !path.join("COMMAND.md").exists() {
                continue;
            }
            let raw = match std::fs::read_to_string(path.join("meta.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<RawMeta>(&s).ok())
            {
                Some(r) => r,
                None => { warn!(command = %name, "skipping command: missing/invalid meta.json"); continue; }
            };
            if !raw.enabled { continue; }
            out.push(CommandInfo {
                name,
                description: raw.description,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Resolve a command by name (case-insensitive), loading its template body.
    /// Returns `None` when the command does not exist, is disabled, or is reserved.
    fn resolve(&self, name: &str) -> Option<ResolvedCommand> {
        if Self::is_reserved(name) { return None; }
        let name = name.to_ascii_lowercase();
        let base = std::path::Path::new(COMMANDS_DIR).join(&name);
        let raw: RawMeta = std::fs::read_to_string(base.join("meta.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())?;
        if !raw.enabled { return None; }
        let template = std::fs::read_to_string(base.join("COMMAND.md")).ok()?;
        Some(ResolvedCommand { name, template })
    }
}
