use serde::Deserialize;

pub use core_api::provider::LlmStrength;

// ── Core config types ─────────────────────────────────────────────────────────

/// LLM runtime settings (clients are managed via LlmManager / DB, not here).
#[derive(Debug, Deserialize)]
pub struct LlmConfig {
    /// Hard cap on the number of history messages projected into the context,
    /// applied as a **sliding tail window**. Omit (the default) to disable it:
    /// once history exceeds the cap, every turn shifts the window's start, which
    /// changes the prompt prefix and costs a full prompt-cache miss on every
    /// single request — while dropping the oldest messages with no summary to
    /// stand in for them. Set it only when a hard message bound is worth both.
    #[serde(default)]
    pub max_history_messages:  Option<usize>,
    pub max_tool_rounds:       Option<usize>,
    /// Maximum number of synchronous sub-agents run concurrently when the LLM emits
    /// a homogeneous batch of sub-agent calls in one response. Omit to use the
    /// default (`DEFAULT_MAX_PARALLEL_SUBAGENTS`). `1` forces sequential dispatch.
    #[serde(default)]
    pub max_parallel_subagents: Option<usize>,
    /// When set, tool results from previous turns that exceed this many characters are
    /// replaced at context-build time with a short placeholder. The original result is
    /// always preserved in the database (and shown in the frontend); only what the LLM
    /// sees in subsequent turns is affected. Omit or set to `null` to disable.
    pub max_tool_result_chars: Option<usize>,
    /// Request/response logging configuration. Omit or set `enabled: false` to disable.
    pub requests_log:          Option<LlmRequestsLogConfig>,
    /// Context compaction settings. Omitting the section leaves manual `/compact`
    /// working on defaults — only the automatic trigger is opt-in, see
    /// [`CompactionConfig::threshold_tokens`].
    #[serde(default)]
    pub compaction:            CompactionConfig,
    /// Controls how the current date/time is injected into each LLM request.
    #[serde(default)]
    pub datetime:              DatetimeConfig,
}

/// Controls date/time injection in the dynamic tail of each LLM request.
///
/// The injected time is **always** truncated to the hour, and the block says so:
/// see [`crate::loop_adapters::system`]. There is deliberately no rounding knob —
/// the granularity is part of what the model is told, not an instance setting.
#[derive(Debug, Clone, Deserialize)]
pub struct DatetimeConfig {
    /// Inject the current date/time into the LLM context. Default: true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// IANA timezone name to use when formatting the injected timestamp.
    /// Populated at startup from the global `timezone` config field.
    #[serde(skip)]
    pub timezone: Option<String>,
}

impl Default for DatetimeConfig {
    fn default() -> Self {
        Self { enabled: true, timezone: None }
    }
}

/// Context compaction: summarises conversation history so the context stops
/// growing.
///
/// The compactor is **always built** — `/compact` is a manual command and must
/// work out of the box. This struct only tunes it, and `threshold_tokens` is
/// the one switch that arms the *automatic* trigger.
#[derive(Debug, Clone, Deserialize)]
pub struct CompactionConfig {
    /// Trigger compaction when the previous turn consumed more than this many
    /// input tokens. Omit (the default) to leave automatic compaction **off**:
    /// history is then append-only, which is what keeps the prompt prefix — and
    /// so the provider's prompt cache — stable across a whole conversation.
    /// Manual `/compact` is unaffected either way.
    #[serde(default)]
    pub threshold_tokens: Option<u32>,
    /// Number of recent messages to keep outside the summary. Defaults to 6.
    #[serde(default = "default_keep_recent")]
    pub keep_recent: usize,
    /// Minimum LLM strength to use for generating summaries via AUTO selection.
    pub strength: Option<LlmStrength>,
}

/// Hand-written rather than derived: a derived `Default` would give
/// `keep_recent: 0`, silently compacting away every recent message on any box
/// that omits the section — which is now the shipped default.
impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            threshold_tokens: None,
            keep_recent:      default_keep_recent(),
            strength:         None,
        }
    }
}

/// Event-triage background processor settings.
#[derive(Debug, Clone, Deserialize)]
pub struct EventTriageConfig {
    /// Interval between ticks, in seconds. Default: 900 (15 minutes).
    #[serde(default = "default_event_triage_interval_secs")]
    pub interval_secs: u64,
    /// Maximum number of events processed per tick. Default: 50.
    #[serde(default = "default_event_triage_batch_size")]
    pub batch_size: i64,
}

impl Default for EventTriageConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_event_triage_interval_secs(),
            batch_size:    default_event_triage_batch_size(),
        }
    }
}

/// Cron scheduler settings.
#[derive(Debug, Default, Deserialize)]
pub struct CronConfig {}

/// Settings for the LLM request/response log (table `llm_requests`).
#[derive(Debug, Clone, Deserialize)]
pub struct LlmRequestsLogConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub request_payload_save: bool,
    #[serde(default = "default_true")]
    pub response_payload_save: bool,
    #[serde(default = "default_true")]
    pub request_header_save: bool,
    #[serde(default = "default_true")]
    pub response_header_save: bool,
    pub cleanup_request_payload_after:  Option<u32>,
    pub cleanup_response_payload_after: Option<u32>,
    pub cleanup_headers_after:          Option<u32>,
    pub cleanup_rows_after:             Option<u32>,
}

fn default_true()             -> bool { true }
fn default_keep_recent()      -> usize { 6 }
fn default_event_triage_interval_secs() -> u64  { 900 }
fn default_event_triage_batch_size()    -> i64  { 50  }

// ── CoreConfig ────────────────────────────────────────────────────────────────

/// Core application config — passed to `Skald::new()`.
/// No HTTP/server knowledge. Derived from `Config` via `Config::into_split()`.
pub struct CoreConfig {
    pub llm:      LlmConfig,
    pub event_triage: EventTriageConfig,
    pub cron:     CronConfig,
    pub timezone: Option<String>,
}
