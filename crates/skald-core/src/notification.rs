use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A structured notification produced by a background agent (event triage) or the cron
/// runner and delivered to the user's home conversation through `ChatHub`.
///
/// This replaces the previous free-text `String` briefing. Carrying `source`,
/// `event_type`, `event_time` and an open `refs` bag preserves the structured
/// context that already exists in `mcp_events` all the way to the main agent —
/// which is then the sole party responsible for the user-facing wording. The
/// `summary` is a neutral, third-person statement of fact, not a message
/// addressed to the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    /// Origin of the event: `"gmail"` | `"whatsapp"` | `"gcal"` | `"cron"` | `"system"`.
    pub source: String,
    /// Event kind, e.g. `"new_email"` | `"whatsapp_message"` | `"cron_result"`.
    pub event_type: String,
    /// Neutral, third-person factual summary of the event. NOT a message to the
    /// user — the main agent phrases the user-facing message from this.
    pub summary: String,
    /// ISO 8601 timestamp of the underlying event.
    pub event_time: String,
    /// Open bag of actionable references (`message_id`, `thread_id`, `from`, …).
    /// Defaults to an empty object when absent, keeping the shape forward-compatible.
    #[serde(default)]
    pub refs: Value,
}
