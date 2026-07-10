pub mod api;
pub mod config;
pub mod server;

use std::sync::Arc;

use anyhow::Result;
use sqlx::SqlitePool;
use tracing::{error, info};

use core_api::plugin::RouterFactory;
use crate::frontend::config::FrontendConfig;
use skald_core::skald::Skald;
use crate::frontend::server::{WebServer, WebServerHandle};

pub struct WebFrontend {
    skald:      Arc<Skald>,
    pub db:     Arc<SqlitePool>,
    static_dir: String,
    port:       u16,
}

impl WebFrontend {
    pub fn new(skald: Arc<Skald>, db: Arc<SqlitePool>, config: &FrontendConfig) -> Self {
        Self {
            port:       config.server.port,
            static_dir: config.web.static_dir.clone(),
            skald,
            db,
        }
    }

    /// Builds the Axum router factory closure used by remote-connectivity plugins.
    fn make_router_factory(&self) -> RouterFactory {
        let skald      = Arc::clone(&self.skald);
        let static_dir = self.static_dir.clone();
        Arc::new(move || {
            WebServer::build_router(&static_dir, Arc::clone(&skald))
        })
    }

    pub async fn start(self) -> Result<WebServerHandle> {
        // Provide the router factory and web port to plugins before start_enabled().
        self.skald.plugin_manager().set_router_factory(self.make_router_factory());
        self.skald.plugin_manager().set_web_port(self.port);

        if let Err(e) = self.skald.plugin_manager().start_enabled().await {
            error!(error = %e, "plugin startup error");
        }
        self.skald.plugin_manager()
            .start_config_watcher(self.skald.shutdown_token().clone());

        // Collect HTTP routers contributed by enabled plugins (plugin.md §12.3)
        // AFTER start_enabled(), so each router can close over state set up during
        // the plugin's reload/start. Nested under /api/plugin/<id>/.
        let plugin_routers = self.skald.plugin_manager().collect_plugin_routers().await;

        let addr = format!("{}:{}", "0.0.0.0", self.port);
        let server = WebServer::new(
            self.static_dir.clone(),
            Arc::clone(&self.skald),
            plugin_routers,
        );
        let handle = server.start(&addr).await?;
        info!(%addr, "server listening");
        skald_core::boot::ready(format!("Ready — http://localhost:{}", self.port));
        Ok(handle)
    }
}
