//! Skald Remote Control relay server — binary entrypoint (see data/ios-app/relay.md).
//!
//! Thin wrapper: load config → build [`AppState`] → start the GC task → serve
//! the axum router until a shutdown signal arrives. All logic lives in the lib.

use std::net::SocketAddr;

use skald_relay_server::config::Config;
use skald_relay_server::{AppState, router, shutdown_signal, spawn_gc};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Persist logs to `logs/skald-relay.log` (rolling daily), mirroring the main
    // app, and also mirror to stdout for terminal development. Raise verbosity
    // with RUST_LOG, e.g. `RUST_LOG=skald_relay_server=debug` (or `=trace` for
    // full frame-level tracing). The `_log_guard` must live for the whole
    // program so the non-blocking writer flushes.
    std::fs::create_dir_all("logs")?;
    let file_appender = tracing_appender::rolling::daily("logs", "skald-relay.log");
    let (non_blocking, _log_guard) = tracing_appender::non_blocking(file_appender);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "skald_relay_server=info,info".into());

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cfg = Config::from_env();
    let bind = cfg.bind;
    let db_path = cfg.db_path.clone();

    let state = AppState::build(cfg).await?;
    tracing::info!(db = %db_path, "store ready");

    spawn_gc(state.clone());

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "relay listening on /v1/ws + /v1/pipe");

    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    tracing::info!("relay stopped");
    Ok(())
}
