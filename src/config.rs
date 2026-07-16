use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

pub use core_api::provider::LlmStrength;
pub use skald_core::config::{
    LlmConfig, TicConfig, CronConfig,
    CompactionConfig, DatetimeConfig, LlmRequestsLogConfig,
};

const DEFAULT_CONFIG: &str = "default.config.yaml";
const CONFIG: &str = "config.yml";

/// Default config baked into the binary at compile time.
///
/// Used by [`bootstrap_data_dir`] (desktop mode) to seed `config.yml` on first
/// launch, where the bundled binary cannot rely on `default.config.yaml` being
/// next to it on disk (the cwd has already been relocated to the per-user data
/// dir). Headless mode still copies `default.config.yaml` from the source tree
/// as before.
const DEFAULT_CONFIG_EMBEDDED: &str = include_str!("../default.config.yaml");

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server:      ServerConfig,
    pub web:         WebConfig,
    pub llm:         LlmConfig,
    #[serde(default)]
    pub marketplace: MarketplaceConfig,
    #[serde(default)]
    pub tic:      TicConfig,
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
                tic:      self.tic,
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

/// Absolute directory for log files.
///
/// `init_logging()` runs **before** [`bootstrap_data_dir`] relocates the cwd, so
/// a relative `"logs"` path would resolve against the bundle's launch cwd (`/`
/// for a Finder-launched `.app`) — un-writable, so the log folder stays empty.
/// In a packaged bundle, return an absolute path under the per-user data dir
/// instead. In every other mode (headless, desktop dev) keep the historical
/// relative `"logs"`, unchanged.
pub fn resolved_log_dir() -> std::path::PathBuf {
    #[cfg(feature = "desktop")]
    {
        if running_from_bundle() {
            if let Some(dir) = dirs::data_dir() {
                return dir.join("Skald").join("logs");
            }
        }
    }
    std::path::PathBuf::from("logs")
}

/// In desktop mode (Tauri bundle), relocate the process working directory to
/// the OS-appropriate per-user data dir so that every relative path in
/// `config.yml` (db, logs, data, secrets, models, agents, …) resolves there
/// instead of `/` (the default cwd of a `.app` bundle on macOS, or the Windows
/// equivalent). Also seeds `config.yml` from the bundled default if missing.
///
/// | OS      | Location                                            |
/// |---------|-----------------------------------------------------|
/// | macOS   | `~/Library/Application Support/Skald`               |
/// | Windows | `%APPDATA%\Skald` (= `C:\Users\<u>\AppData\Roaming`)|
/// | Linux   | `~/.local/share/Skald`                              |
///
/// ## When relocation happens
/// Only when the process is running from a packaged bundle (e.g. inside
/// `Skald.app/Contents/MacOS/`). In dev mode (`cargo run --features desktop`),
/// the cwd is left untouched so all source-tree assets (`agents/`, `skills/`,
/// `web/`, `config.yml`, …) keep resolving from the crate root as in headless
/// mode.
///
/// In headless mode this is always a no-op: the cwd stays as the user launched
/// it, preserving today's behaviour (`./database.db`, `./logs/`, …).
#[cfg(feature = "desktop")]
pub fn bootstrap_data_dir() -> Result<()> {
    use tracing::info;
    if !running_from_bundle() {
        info!("desktop mode (dev): cwd unchanged — using source-tree assets");
        return Ok(());
    }
    let data_dir = dirs::data_dir()
        .context("could not determine OS data directory")?
        .join("Skald");
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("failed to create data dir at {}", data_dir.display()))?;
    info!(path = %data_dir.display(), "desktop mode: relocating cwd to per-user data dir");
    std::env::set_current_dir(&data_dir)
        .with_context(|| format!("failed to cd to {}", data_dir.display()))?;

    // Seed config.yml from the bundled default if absent (first launch).
    let config_path = Path::new(CONFIG);
    if !config_path.exists() {
        let _ = std::fs::write(CONFIG, DEFAULT_CONFIG_EMBEDDED);
        info!(path = %data_dir.display(), "seeded config.yml from embedded default");
    }

    // Make the read-only bundled assets reachable from the relocated cwd.
    link_bundled_assets(&data_dir)?;
    Ok(())
}

/// Read-only asset directories shipped inside the `.app` bundle's `Resources/`
/// dir (see `tauri.conf.json > bundle > resources`). The backend looks these up
/// by relative path from the cwd (agent discovery reads `agents/`, Axum serves
/// `web/`, etc.), but the cwd has just been relocated to the data dir — where
/// they don't exist. Without this, `Skald::new` fails with
/// "Failed to read agents directory 'agents'" and the app exits on launch.
#[cfg(feature = "desktop")]
const BUNDLED_ASSETS: &[&str] = &["agents", "web", "skills", "commands"];

/// (Re)link each bundled asset dir into the per-user data dir as a symlink to
/// the copy inside the app bundle's `Resources/`, so the existing relative-path
/// lookups resolve while mutable state (db, config, logs, secrets) stays in the
/// data dir itself.
///
/// Symlinking (rather than copying) keeps the assets in sync with the installed
/// app version automatically. A pre-existing **real** directory is treated as a
/// user override and left untouched; only symlinks are refreshed.
#[cfg(feature = "desktop")]
fn link_bundled_assets(data_dir: &Path) -> Result<()> {
    use tracing::{info, warn};
    let exe = std::env::current_exe().context("could not resolve current_exe")?;
    // `.../Skald.app/Contents/MacOS/skald` → `.../Skald.app/Contents/Resources`
    let resource_dir = match exe.parent().and_then(|p| p.parent()) {
        Some(contents) => contents.join("Resources"),
        None => {
            warn!("could not derive bundle Resources dir from exe path — skipping asset link");
            return Ok(());
        }
    };
    for name in BUNDLED_ASSETS {
        let src = resource_dir.join(name);
        if !src.exists() {
            warn!(asset = name, "bundled asset missing from Resources — skipping");
            continue;
        }
        let dst = data_dir.join(name);
        match std::fs::symlink_metadata(&dst) {
            // Stale symlink from a previous launch — replace it.
            Ok(meta) if meta.file_type().is_symlink() => { let _ = std::fs::remove_file(&dst); }
            // A real dir/file the user created — respect it, don't clobber.
            Ok(_) => continue,
            // Absent — fall through and create.
            Err(_) => {}
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&src, &dst)
            .with_context(|| format!("failed to symlink {} -> {}", dst.display(), src.display()))?;
        info!(asset = name, target = %src.display(), "linked bundled asset into data dir");
    }
    Ok(())
}

/// Heuristic: are we running inside a packaged bundle (e.g. `Foo.app`)?
/// Used to decide whether to relocate the cwd to the per-user data dir.
#[cfg(feature = "desktop")]
fn running_from_bundle() -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Ok(exe) = std::env::current_exe() {
            return exe.to_string_lossy().contains(".app/");
        }
    }
    // Windows / Linux: TBD when packaging targets land. For now treat all
    // launches as dev mode (no cwd relocation).
    false
}

#[cfg(not(feature = "desktop"))]
pub fn bootstrap_data_dir() -> Result<()> {
    // Headless mode: keep cwd as launched, no relocation.
    Ok(())
}
