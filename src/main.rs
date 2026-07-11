mod boot_format;
#[cfg(feature = "desktop")]
mod desktop;
mod frontend;
mod config;

// The core lives in `crates/skald-core`. This binary is one shell around it;
// `skald-setup` is another.
use skald_core::boot;

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use tracing::level_filters::LevelFilter;
use tracing::{debug, error, info, warn};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use core_api::plugin::Plugin;
use config::Config;
use skald_core::db::{SYSTEM_DB_PATH, init_system_pool};
use skald_core::skald::Skald;
use crate::frontend::WebFrontend;
use crate::frontend::server::WebServerHandle;

const APP_NAME: &str = env!("CARGO_PKG_NAME");

/// Backend handle — everything that must live until shutdown.
///
/// Constructed by [`run_backend`], consumed by [`shutdown_backend`]. In
/// headless mode it lives in `async_main()`; in desktop mode it's stashed in
/// Tauri's managed state (`app.manage(backend)`) and consumed on Quit.
pub struct Backend {
    pub skald: Arc<Skald>,
    pub web:   WebServerHandle,
    pub pool:  Arc<SqlitePool>,
}

fn main() -> Result<()> {
    // Install the rustls crypto provider (ring) before any TLS handshake.
    // Required because reqwest is built with `rustls-no-provider` (see
    // Cargo.toml): exactly one process-wide provider must be installed before
    // the first Client is built. In headless mode this happened to work
    // because the first HTTPS request was lazy; in desktop mode the backend
    // task fires requests earlier, so install it explicitly up front.
    rustls::crypto::ring::default_provider().install_default()
        .expect("failed to install rustls ring crypto provider");

    init_logging();

    #[cfg(feature = "desktop")]
    {
        desktop::run()
    }
    #[cfg(not(feature = "desktop"))]
    {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(async_main())
    }
}

/// Initialise tracing (file + boot stdout layers) and the panic hook.
///
/// Called once at process start, before either the tokio runtime (headless) or
/// the Tauri event loop (desktop). Not dependent on any async runtime.
fn init_logging() {
    let log_dir = config::resolved_log_dir();
    std::fs::create_dir_all(&log_dir).ok();
    let file_appender = tracing_appender::rolling::daily(&log_dir, format!("{APP_NAME}.log"));
    let (non_blocking, _log_guard) = tracing_appender::non_blocking(file_appender);
    // The worker thread behind `non_blocking` must outlive any shutdown path;
    // intentionally leak the guard so the writer is never dropped mid-process.
    // Logs are flushed by the rolling appender's own background thread.
    std::mem::forget(_log_guard);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // File layer: full structured logs, governed by RUST_LOG.
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_filter(env_filter);

    // Stdout layer: only the curated `boot` target, rendered cleanly. Its own
    // target filter makes it independent of RUST_LOG, so bootstrap always shows.
    // ANSI is enabled only on a real terminal.
    let boot_layer = tracing_subscriber::fmt::layer()
        .event_format(boot_format::BootFormat)
        .with_writer(std::io::stdout)
        .with_ansi(std::io::stdout().is_terminal())
        .with_filter(Targets::new().with_target(boot::TARGET, LevelFilter::TRACE));

    tracing_subscriber::registry()
        .with(file_layer)
        .with(boot_layer)
        .init();

    // Route panics through tracing so they land in logs/ (the default hook only
    // writes to stderr, invisible under supervisors / Tauri). Chain to the
    // default hook so the human-readable message + backtrace still print.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map(|l| l.to_string()).unwrap_or_else(|| "unknown".into());
        let msg = info.payload().downcast_ref::<&str>().map(|s| (*s).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        error!(target: "panic", location = %location, message = %msg, "thread panicked");
        default_panic(info);
    }));
}

/// Headless entry point (no Tauri): run the backend, wait for a shutdown
/// signal, then shut everything down. Used only in `cfg(not(feature = "desktop"))`.
async fn async_main() -> Result<()> {
    info!(version = env!("CARGO_PKG_VERSION"), "starting {APP_NAME}");
    boot::title(format!("{APP_NAME} v{} — starting", env!("CARGO_PKG_VERSION")));

    let backend = run_backend().await?;

    let signal = wait_for_shutdown_signal().await;
    warn!(signal, "shutdown signal received — shutting down");

    shutdown_backend(backend).await;
    info!("shutdown complete");
    Ok(())
}

/// Boot the Skald backend: load config, build plugins, open the DB pool,
/// construct `Skald`, and start the web frontend. Returns a [`Backend`] whose
/// components must be shut down via [`shutdown_backend`] for graceful exit.
///
/// Shared by both the headless entry point and the desktop (Tauri) setup hook.
pub async fn run_backend() -> Result<Backend> {
    // In desktop mode, relocate the process cwd to the OS-appropriate per-user
    // data dir before reading any relative path (db, logs, data, …). Headless
    // mode keeps the cwd unchanged.
    config::bootstrap_data_dir()?;

    let cfg = match Config::load() {
        Ok(c)  => { debug!("config loaded"); c }
        Err(e) => { error!(error = %e, "failed to load config"); return Err(e); }
    };
    let (core_cfg, frontend_cfg) = cfg.into_split();

    let plugins = build_plugins();

    let pool = Arc::new(init_system_pool(SYSTEM_DB_PATH).await?);
    info!(path = SYSTEM_DB_PATH, "database ready");

    let skald = Skald::new(Arc::clone(&pool), &core_cfg, plugins).await?;

    let handle = WebFrontend::new(skald.clone(), Arc::clone(&pool), &frontend_cfg)
        .start().await?;

    Ok(Backend { skald, web: handle, pool })
}

/// Build the plugin list. Extracted so both entry points share the same set.
fn build_plugins() -> Vec<Arc<dyn Plugin>> {
    let mut plugins: Vec<Arc<dyn Plugin>> = vec![
        Arc::new(plugin_tailscale_remote::RemotePlugin::new()),
        Arc::new(plugin_telegram_bot::TelegramPlugin::new()),
        Arc::new(plugin_mobile_connector::MobileConnectorPlugin::new()),
        Arc::new(plugin_comfyui::ComfyUIPlugin::new()),
        Arc::new(plugin_tts_orpheus_3b::OrpheusTtsPlugin::new()),
        Arc::new(plugin_tts_kokoro::KokoroTtsPlugin::new()),
        Arc::new(plugin_elevenlabs::ElevenLabsPlugin::new()),
    ];
    #[cfg(feature = "whisper-local")]
    plugins.push(Arc::new(plugin_transcribe_whisper_local::WhisperLocalPlugin::new()));
    plugins
}

/// Graceful shutdown of the backend: HTTP server, Skald managers, DB pool.
/// Order matters: web first (stop accepting requests), then skald (cancel
/// background tasks), then DB pool.
pub async fn shutdown_backend(backend: Backend) {
    backend.web.shutdown().await;
    backend.skald.shutdown().await;
    backend.pool.close().await;
}

/// Wait for an OS shutdown signal and return its name for logging.
///
/// We trap **both** SIGINT (Ctrl+C) and SIGTERM. Without an explicit SIGTERM
/// handler the default action kills the process with exit code 143, which the
/// `run.sh` supervisor treats as a hard stop (only exit 255 triggers a
/// restart) — and the kill leaves no trace in the log. Trapping it lets us log
/// the cause and shut down gracefully (exit 0).
#[cfg(unix)]
async fn wait_for_shutdown_signal() -> &'static str {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => "SIGTERM",
        _ = sigint.recv()  => "SIGINT",
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown_signal() -> &'static str {
    let _ = tokio::signal::ctrl_c().await;
    "CTRL_C"
}
