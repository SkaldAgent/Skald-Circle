use std::collections::HashMap;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::events::GlobalEvent;
use crate::interface_tool::InterfaceTool;
use crate::message_meta::MessageMetadata;

// ── SendMessageOptions ────────────────────────────────────────────────────────

/// Optional parameters for a [`ChatHubApi::send_message`] call.
#[derive(Default)]
pub struct SendMessageOptions {
    /// Agent to use for this source's session. Defaults to `"main"` if not set.
    /// Only takes effect when a new session is created — ignored for existing sessions.
    pub agent_id: Option<String>,
    /// Named substitutions applied to the agent's system prompt.
    /// Each entry replaces the sentinel `__KEY__` in the loaded prompt text.
    /// Matches `<!-- KEY -->` placeholders that `agents::resolve_includes` converts to sentinels.
    pub system_substitutions: HashMap<String, String>,
    pub client_name: Option<String>,
    /// Extra text prepended to the agent's system prompt for this turn only.
    /// STATIC: safe to cache — use for interface-specific formatting rules that
    /// never change turn-to-turn (e.g. Telegram HTML mode).
    pub extra_system_context: Option<String>,
    /// Extra system message injected AFTER the conversation history for this turn only.
    /// DYNAMIC: not cached — use for per-turn context (e.g. notification framing).
    pub extra_system_dynamic: Option<String>,
    /// Short reminder injected near the tail of the message list to prevent drift.
    pub tail_reminder: Option<String>,
    pub interface_tools: Vec<InterfaceTool>,
    /// True for system-generated messages injected as user turns (notification briefings).
    pub is_synthetic: bool,
    /// Opaque structured metadata persisted on the user turn (e.g. file attachments).
    /// ChatHub forwards it verbatim; the MessageBuilder/UI derive their own views.
    pub metadata: Option<MessageMetadata>,
}

// ── ChatHubApi ────────────────────────────────────────────────────────────────

/// Abstraction over [`ChatHub`](crate) that plugins and external crates depend on.
///
/// Implementing this trait for `ChatHub` in the main crate is the only coupling
/// point needed: plugins can accept `Arc<dyn ChatHubApi>` and stay independent.
#[async_trait]
pub trait ChatHubApi: Send + Sync {
    /// Register a source. No-op for duplicate registrations.
    async fn register(&self, source_id: &str);

    /// Send a user message for a source, running a full LLM turn.
    /// Creates a session lazily if none exists yet.
    async fn send_message(
        &self,
        source_id: &str,
        prompt: &str,
        opts: SendMessageOptions,
    ) -> anyhow::Result<()>;

    /// Create a new session for the source, discarding the previous one.
    async fn clear(&self, source_id: &str) -> anyhow::Result<i64>;

    /// Subscribe to the global event bus.
    /// Filtering by source is the caller's responsibility.
    fn events(&self, source_id: &str) -> broadcast::Receiver<GlobalEvent>;

    /// Set which source is the "home" for background agent notifications.
    async fn set_home(&self, source_id: &str) -> anyhow::Result<()>;

    /// Returns token usage `(input, output)` for the last message in the source's session.
    async fn context_info(
        &self,
        source_id: &str,
    ) -> anyhow::Result<(Option<i64>, Option<i64>)>;

    /// Total spend (USD) of the source's active session, including synchronous
    /// sub-agent frames and excluding asynchronous tasks (which run in their own
    /// session). `None` when no provider reported a cost.
    async fn cost_info(&self, source_id: &str) -> anyhow::Result<Option<f64>>;

    /// Force compaction of the source's active session history.
    /// Returns `true` if compaction occurred.
    async fn force_compact(&self, source_id: &str) -> anyhow::Result<bool>;

    /// Resume any interrupted turn for a source's active session.
    async fn resume(&self, source_id: &str) -> anyhow::Result<()>;

    /// Approve a pending tool-call approval request.
    async fn approve(&self, request_id: i64);

    /// Reject a pending tool-call approval request.
    async fn reject(&self, request_id: i64, note: String);

    /// Resolve a pending `ask_user_clarification` question.
    /// Collapses `session_handler(source_id).resolve_question(...)` into a single
    /// hub-level call so callers never need to know about `ChatSessionHandler`.
    async fn resolve_question(&self, source_id: &str, request_id: i64, answer: String);

    /// Cancel the active LLM turn for a source, clearing any pending approvals
    /// and clarification questions. No-op if no session is active.
    async fn cancel(&self, source_id: &str);

    /// Revoke all session-scoped MCP grants for a source's active session.
    /// The next LLM turn will start with no MCP tools activated.
    async fn reset_mcp(&self, source_id: &str) -> anyhow::Result<()>;

    /// Returns `(models, default)` where `models` is the ordered list of usable
    /// LLM client names (first entry is always `"auto"`) and `default` is the
    /// configured default client name. Used by `/models` and the web selector.
    async fn list_clients(&self) -> (Vec<String>, String);

    /// Returns the client name pinned for the source, or `None` when unset
    /// (the caller should fall back to AUTO resolution).
    async fn get_selected_client(&self, source_id: &str) -> Option<String>;

    /// Pin a client name for the source and broadcast `ClientSelected` to every
    /// client of the source. `client` must be a `list_clients()` entry (e.g.
    /// `"auto"` or a model name); the caller is responsible for validation.
    async fn set_selected_client(&self, source_id: &str, client: String);

    /// Clear any pinned client for the source (revert to AUTO) and broadcast
    /// `ClientSelected { client: "auto" }`.
    async fn clear_selected_client(&self, source_id: &str);

    /// Snapshot of the model list with the per-source current selection marked.
    /// Returns `(index, name, is_current)` tuples — call sites format them as
    /// HTML or Markdown without re-querying the LLM manager.
    async fn list_clients_marked(
        &self,
        source_id: &str,
    ) -> Vec<(usize, String, bool)>;

    /// Apply a `/model {arg}` command: resolve the argument, mutate the
    /// per-source pinned client (broadcasting `ClientSelected`), return a
    /// structured outcome the caller can format for its medium (HTML/Markdown).
    async fn apply_model_command(
        &self,
        source_id: &str,
        arg: &str,
    ) -> ModelCommandOutcome;
}

// ── Model command helpers (shared business logic) ────────────────────────────

/// Outcome of [`ChatHubApi::apply_model_command`]. The caller formats each
/// variant for its medium (Telegram HTML, web Markdown, …).
#[derive(Debug, Clone)]
pub enum ModelCommandOutcome {
    /// A model was pinned. The backend has already broadcast `ClientSelected`.
    Set(String),
    /// The pin was cleared (back to AUTO). The backend has already broadcast
    /// `ClientSelected { client: "auto" }`.
    Cleared,
    /// The argument was empty, out of range, or ambiguous. Carries a
    /// user-facing message (no formatting — the caller wraps it as needed).
    Error(String),
}

/// Resolve a `/model` (or analogous — e.g. a future `/reasoning`) argument
/// against an ordered list.
///
/// Returns:
/// - `Ok(Some(client))` for a unique match (caller pins it)
/// - `Ok(None)` for `auto` / index 0 (caller clears the pin)
/// - `Err(user_facing_message)` when the input is empty / out of range /
///   ambiguous
///
/// Accepts (in order):
/// 1. `auto` (case-insensitive) — or numeric `0` which is conventionally the
///    "auto" slot in `client_names()`
/// 2. Numeric index `N` → exact lookup
/// 3. Exact case-insensitive name match
/// 4. Substring match (case-insensitive) — must be unique
pub fn resolve_list_arg(models: &[String], arg: &str) -> Result<Option<String>, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Err("Usage: /model N or /model name or /model auto".to_string());
    }
    if arg.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    if let Ok(n) = arg.parse::<usize>() {
        return match models.get(n) {
            Some(m) if m == "auto" => Ok(None),
            Some(m)                => Ok(Some(m.clone())),
            None                   => Err(format!("Index {n} out of range. Use /models to see the list.")),
        };
    }
    if let Some(m) = models.iter().find(|m| m.eq_ignore_ascii_case(arg)) {
        return Ok(if m == "auto" { None } else { Some(m.clone()) });
    }
    let lower = arg.to_ascii_lowercase();
    let hits: Vec<&String> = models.iter().filter(|m| m.to_ascii_lowercase().contains(&lower)).collect();
    match hits.len() {
        1 => Ok(if hits[0] == "auto" { None } else { Some(hits[0].clone()) }),
        0 => Err(format!("No model matches '{arg}'. Use /models to see the list.")),
        _ => Err(format!(
            "Multiple models match '{arg}': {}. Be more specific.",
            hits.iter().map(|h| h.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}
