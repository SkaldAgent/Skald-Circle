use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

pub use core_api::provider::LlmStrength;
pub use skald_core::config::{
    LlmConfig, EventTriageConfig, CronConfig,
    CompactionConfig, DatetimeConfig, LlmRequestsLogConfig,
};

const DEFAULT_CONFIG: &str = "default.config.yaml";
const CONFIG: &str = "config.yml";

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server:      ServerConfig,
    pub web:         WebConfig,
    pub llm:         LlmConfig,
    #[serde(default)]
    pub marketplace: MarketplaceConfig,
    #[serde(default)]
    pub event_triage: EventTriageConfig,
    #[serde(default)]
    pub cron:     CronConfig,
    /// Global IANA timezone name (e.g. `"Europe/Rome"`).
    /// Applied to: cron expression evaluation, datetime injected into the LLM context.
    /// When omitted, the server's local system timezone is used everywhere.
    pub timezone: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Deserialize)]
pub struct WebConfig {
    pub static_dir: String,
}

/// The connector marketplace feed (blueprint §14/§15).
///
/// Configurable, not hardcoded: an on-premise product must not hard-require
/// reaching one vendor's host. Point it at a self-hosted mirror, or an offline
/// copy served locally, and nothing else changes.
#[derive(Debug, Deserialize)]
pub struct MarketplaceConfig {
    /// Base URL serving `connectors.json` and each `<folder>/connector.json`.
    pub url: String,
}

impl Default for MarketplaceConfig {
    fn default() -> Self {
        Self { url: "https://connectors.skaldagent.net".to_string() }
    }
}

impl Config {
    pub fn into_split(self) -> (skald_core::config::CoreConfig, crate::frontend::config::FrontendConfig) {
        let tz = self.timezone.clone();
        (
            skald_core::config::CoreConfig {
                llm:      self.llm,
                event_triage: self.event_triage,
                cron:     self.cron,
                timezone: self.timezone,
            },
            crate::frontend::config::FrontendConfig {
                server:      self.server,
                web:         self.web,
                marketplace: self.marketplace,
                timezone:    tz,
            },
        )
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path  = Path::new(CONFIG);
        let default_path = Path::new(DEFAULT_CONFIG);

        if !config_path.exists() {
            std::fs::copy(default_path, config_path)
                .with_context(|| format!("Failed to copy {DEFAULT_CONFIG} to {CONFIG}"))?;
            skald_core::boot::section(format!("Created {CONFIG} from {DEFAULT_CONFIG}"));
        }

        let content = std::fs::read_to_string(config_path)
            .with_context(|| format!("Failed to read {CONFIG}"))?;

        serde_yaml::from_str(&content).with_context(|| format!("Failed to parse {CONFIG}"))
    }
}

/// Directory for log files: a relative `"logs"` under the launch cwd.
pub fn resolved_log_dir() -> std::path::PathBuf {
    std::path::PathBuf::from("logs")
}
