use crate::config::{ServerConfig, WebConfig};

/// Web frontend config — passed to `WebFrontend::new()`.
/// Derived from `Config` via `Config::into_split()`.
pub struct FrontendConfig {
    pub server:   ServerConfig,
    pub web:      WebConfig,
    pub timezone: Option<String>,
}
