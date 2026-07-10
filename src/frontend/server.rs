use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use tokio::{net::TcpListener, task::JoinHandle};
use tower_http::compression::CompressionLayer;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use axum::http::{HeaderValue, header};
use tower::ServiceBuilder;

use crate::frontend::api;
use crate::core::skald::Skald;

pub struct WebServer {
    static_dir:     String,
    skald:          Arc<Skald>,
    /// Routers contributed by enabled plugins, nested under `/api/plugin/<id>/`
    /// (plugin.md §12.3). Empty for the mesh-facing router built by the factory.
    plugin_routers: Vec<(String, Router)>,
}

pub struct WebServerHandle {
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    task:        JoinHandle<()>,
}

impl WebServer {
    pub fn new(
        static_dir:     String,
        skald:          Arc<Skald>,
        plugin_routers: Vec<(String, Router)>,
    ) -> Self {
        Self { static_dir, skald, plugin_routers }
    }

    pub async fn start(self, addr: &str) -> Result<WebServerHandle> {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("Failed to bind to {addr}"))?;

        let router = Self::build_router_with_plugins(&self.static_dir, self.skald, self.plugin_routers);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("Web server encountered a fatal error");
        });

        Ok(WebServerHandle { shutdown_tx, task })
    }

    pub fn build_router(static_dir: &str, skald: Arc<Skald>) -> Router {
        Self::build_router_with_plugins(static_dir, skald, Vec::new())
    }

    /// Like [`build_router`], but also nests plugin-contributed routers under
    /// `/api/plugin/<id>/` (plugin.md §12.3). The plugin routers are stateless
    /// (`Router<()>`) — they close over their own state — so they mount cleanly
    /// alongside the state-carrying app routes.
    pub fn build_router_with_plugins(
        static_dir:     &str,
        skald:          Arc<Skald>,
        plugin_routers: Vec<(String, Router)>,
    ) -> Router {
        // Resolve the app state first so the resulting `Router<()>` can host the
        // stateless plugin routers via `nest`.
        let mut router = Router::new()
            .nest("/api", api::router())
            .with_state(skald);
        for (id, plugin_router) in plugin_routers {
            router = router.nest(&format!("/api/plugin/{id}"), plugin_router);
        }
        // Serve the data/ directory under /data/ (accessible via URL).
        let data_dir = Path::new(static_dir).parent().unwrap_or(Path::new(".")).join("data");
        // Static responses (SPA assets + /data) get `Cache-Control: no-cache`:
        // the browser may store them but MUST revalidate before use, so after a
        // self-rewrite/restart the client never serves a stale asset (no heuristic
        // caching). Revalidation yields cheap 304s (the body is already on disk).
        // `/api` is deliberately left without this header (dynamic, not cached).
        let static_assets = || ServiceBuilder::new().layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ));
        router = router.nest_service("/data", static_assets().service(ServeDir::new(&data_dir)));
        router = router.fallback_service(static_assets().service(ServeDir::new(static_dir)));
        // Negotiated gzip/brotli compression (Accept-Encoding). Matters most for
        // the mobile WebView, whose HTTP traffic is reverse-proxied byte-for-byte
        // over a relay pipe — text assets (JS/CSS/HTML) shrink ~70-90%, so far
        // fewer bytes cross the slow link. No-op for already-compressed media and
        // for clients that don't advertise an encoding.
        router.layer(CompressionLayer::new())
    }
}

impl WebServerHandle {
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(());
        let _ = self.task.await;
    }
}
