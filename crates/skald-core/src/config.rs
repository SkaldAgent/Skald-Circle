use serde::Deserialize;

pub use core_api::provider::LlmStrength;

// ── Core config types ─────────────────────────────────────────────────────────

/// LLM runtime settings (clients are managed via LlmManager / DB, not here).
#[derive(Debug, Deserialize)]
pub struct LlmConfig {
    pub max_history_messages:  usize,
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
    /// Context compaction settings. Omit to disable automatic compaction.
    pub compaction:            Option<CompactionConfig>,
    /// Controls how the current date/time is injected into each LLM request.
    #[serde(default)]
    pub datetime:              DatetimeConfig,
}

/// Controls date/time injection in the dynamic tail of each LLM request.
#[derive(Debug, Clone, Deserialize)]
pub struct DatetimeConfig {
    /// Inject the current date/time into the LLM context. Default: true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// When set, round the injected time down to the nearest N-minute boundary.
    pub round_minutes: Option<u32>,
    /// IANA timezone name to use when formatting the injected timestamp.
    /// Populated at startup from the global `timezone` config field.
    #[serde(skip)]
    pub timezone: Option<String>,
}

impl Default for DatetimeConfig {
    fn default() -> Self {
        Self { enabled: true, round_minutes: None, timezone: None }
    }
}

/// Context compaction: summarises conversation history when the LLM context
/// exceeds `threshold_tokens`.
#[derive(Debug, Clone, Deserialize)]
pub struct CompactionConfig {
    /// Trigger compaction when the previous turn consumed more than this many input tokens.
    pub threshold_tokens: u32,
    /// Number of recent messages to keep outside the summary. Defaults to 6.
    #[serde(default = "default_keep_recent")]
    pub keep_recent: usize,
    /// Minimum LLM strength to use for generating summaries via AUTO selection.
    pub strength: Option<LlmStrength>,
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
