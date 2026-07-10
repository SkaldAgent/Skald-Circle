// The manager only ever handles `Arc<dyn Plugin>`. Naming a concrete plugin crate
// here would make the core depend on every plugin — and, through
// `plugin-transcribe-whisper-local`, on a C build — for no gain: the consumer
// constructs the plugin list and passes it to `Skald::new`.
pub use core_api::plugin::{Plugin, PluginContext, RouterFactory};

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const PLUGIN_START_TIMEOUT_SECS: u64 = 30;
const PLUGIN_STOP_TIMEOUT_SECS:  u64 = 5;

use anyhow::Result;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::db::plugins as db;
use crate::skald::Skald;

// ── Public plugin info (returned by list_items tool and REST API) ─────────────

#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub id:             String,
    pub name:           String,
    pub description:    String,
    pub enabled:        bool,
    pub running:        bool,
    pub config:         Value,
    pub config_schema:  Value,
    pub runtime_status: Option<Value>,
}

// ── PluginManager ─────────────────────────────────────────────────────────────

pub struct PluginManager {
    plugins:        Vec<Arc<dyn Plugin>>,
    db:             Arc<SqlitePool>,
    skald:          OnceLock<Arc<Skald>>,
    /// Provided by WebFrontend before start_enabled() is called.
    router_factory: OnceLock<RouterFactory>,
    /// HTTP port the web server is bound to — provided by WebFrontend before start_enabled().
    web_port:       OnceLock<u16>,
    /// Last known (enabled, config_json) per plugin id — used by the watcher.
    known_state:    Mutex<HashMap<String, (bool, String)>>,
}

impl PluginManager {
    pub fn new(db: Arc<SqlitePool>) -> Self {
        Self {
            plugins:        Vec::new(),
            db,
            skald:          OnceLock::new(),
            router_factory: OnceLock::new(),
            web_port:       OnceLock::new(),
            known_state:    Mutex::new(HashMap::new()),
        }
    }

    pub fn register(&mut self, plugin: impl Plugin + 'static) {
        self.plugins.push(Arc::new(plugin));
    }

    pub fn register_arc(&mut self, plugin: Arc<dyn Plugin>) {
        self.plugins.push(plugin);
    }

    pub fn set_skald(&self, skald: Arc<Skald>) {
        let _ = self.skald.set(skald);
    }

    /// Called by WebFrontend before start_enabled().
    pub fn set_router_factory(&self, factory: RouterFactory) {
        let _ = self.router_factory.set(factory);
    }

    /// Called by WebFrontend before start_enabled().
    pub fn set_web_port(&self, port: u16) {
        let _ = self.web_port.set(port);
    }

    fn skald(&self) -> Result<Arc<Skald>> {
        self.skald.get().cloned()
            .ok_or_else(|| anyhow::anyhow!("PluginManager: skald not initialized"))
    }

    fn build_context(&self, skald: &Skald) -> Result<PluginContext> {
        let router_factory = self.router_factory.get().cloned()
            .ok_or_else(|| anyhow::anyhow!("PluginManager: router_factory not set"))?;
        let web_port = self.web_port.get().copied()
            .ok_or_else(|| anyhow::anyhow!("PluginManager: web_port not set"))?;

        Ok(PluginContext {
            chat_hub:                Arc::clone(skald.chat_hub()) as _,
            command:                 Arc::clone(skald.command_manager()) as _,
            approval:                Arc::clone(skald.approval()) as _,
            inbox:                   Arc::new(skald.inbox().clone()) as _,
            db:                      Arc::clone(skald.db()),
            secrets:                 Arc::clone(skald.secrets()) as _,
            transcribe:              Arc::clone(skald.transcribe_manager()) as _,
            transcribe_registry:     Arc::clone(skald.transcribe_manager()) as _,
            image_generate_registry: Arc::clone(skald.image_generator_manager()) as _,
            tts_registry:            Arc::clone(skald.tts_manager()) as _,
            tts_provider:            Arc::clone(skald.tts_manager()) as _,
            api_provider_registry:   Arc::clone(skald.provider_registry()) as _,
            location:                Arc::clone(skald.location_manager()) as _,
            system_bus:              Arc::clone(skald.system_bus()),
            web_port,
            remote_slot:             Arc::clone(skald.remote()),
            router_factory,
        })
    }

    /// Collects the HTTP routers contributed by enabled plugins (plugin.md §12.3).
    /// Returns `(plugin_id, router)` pairs; the caller (`WebFrontend::start`)
    /// nests each under `/api/plugin/<id>/`. Only plugins with `enabled=true` in
    /// the DB and a non-`None` `http_router()` are included.
    ///
    /// Call this AFTER `start_enabled()` so a plugin's router can close over state
    /// initialised during `reload`/`start`.
    pub async fn collect_plugin_routers(&self) -> Vec<(String, axum::Router)> {
        let mut out = Vec::new();
        for plugin in &self.plugins {
            match db::get(&self.db, plugin.id()).await {
                Ok(Some(row)) if row.enabled => {}
                Ok(_) => continue,
                Err(e) => {
                    warn!(plugin = plugin.id(), error = %e, "collect_plugin_routers: DB read failed; skipping");
                    continue;
                }
            }
            if let Some(router) = plugin.http_router() {
                info!(plugin = plugin.id(), "plugin contributed an HTTP router → /api/plugin/{}", plugin.id());
                out.push((plugin.id().to_string(), router));
            }
        }
        out
    }

    // ── Startup ───────────────────────────────────────────────────────────────

    /// Calls reload() for every plugin that has enabled=true in DB.
    /// Plugins without a DB row are skipped (not yet configured).
    /// After each successful start, registers the plugin's Memory backend (if any).
    /// Must be called after both set_skald() and set_router_factory().
    pub async fn start_enabled(&self) -> Result<()> {
        let skald = self.skald()?;
        // Build a full inventory for the bootstrap report: active (started),
        // failed (enabled but errored/timed out), and available (disabled).
        let mut active:   Vec<String> = Vec::new();
        let mut failed:   Vec<(String, String)> = Vec::new();
        let mut disabled: Vec<String> = Vec::new();

        for plugin in &self.plugins {
            let row = db::get(&self.db, plugin.id()).await?;
            let enabled = row.as_ref().map(|r| r.enabled).unwrap_or(false);
            if !enabled {
                disabled.push(plugin.id().to_string());
                continue;
            }
            let row = row.expect("enabled implies row present");
            let config = serde_json::from_str(&row.config).unwrap_or(json!({}));
            let deadline = Duration::from_secs(PLUGIN_START_TIMEOUT_SECS);
            let ctx = self.build_context(&skald)?;
            match timeout(deadline, plugin.reload(true, config, ctx)).await {
                Ok(Ok(())) => {
                    self.known_state.lock().await
                        .insert(plugin.id().to_string(), (true, row.config));
                    info!(plugin = plugin.id(), "plugin started");
                    if let Some(mem) = plugin.memory() {
                        skald.memory_manager().register(mem).await;
                    }
                    active.push(plugin.id().to_string());
                }
                Ok(Err(e)) => {
                    error!(plugin = plugin.id(), error = %e, "plugin failed to start");
                    failed.push((plugin.id().to_string(), e.to_string()));
                }
                Err(_) => {
                    error!(plugin = plugin.id(), secs = PLUGIN_START_TIMEOUT_SECS, "plugin start timed out");
                    failed.push((plugin.id().to_string(),
                        format!("start timed out after {PLUGIN_START_TIMEOUT_SECS}s")));
                }
            }
        }

        crate::boot::section(format!(
            "Plugins — {} active, {} failed, {} available",
            active.len(), failed.len(), disabled.len()
        ));
        if !active.is_empty() {
            crate::boot::ok(active.join(", "));
        }
        for (id, reason) in &failed {
            crate::boot::fail(format!("{id} — {reason}"));
        }
        if !disabled.is_empty() {
            crate::boot::off(disabled.join(", "));
        }

        Ok(())
    }

    pub async fn stop_all(&self) {
        for plugin in &self.plugins {
            if plugin.is_running() {
                let deadline = Duration::from_secs(PLUGIN_STOP_TIMEOUT_SECS);
                match timeout(deadline, plugin.stop()).await {
                    Ok(Ok(()))  => info!(plugin = plugin.id(), "plugin stopped"),
                    Ok(Err(e))  => error!(plugin = plugin.id(), error = %e, "plugin stop error"),
                    Err(_)      => warn!(plugin = plugin.id(), secs = PLUGIN_STOP_TIMEOUT_SECS, "plugin stop timed out"),
                }
            }
        }
    }

    // ── Config update (called by REST API) ────────────────────────────────────

    /// Persists the new config to DB, then calls reload() immediately.
    pub async fn update_config(&self, id: &str, enabled: bool, config: Value) -> Result<()> {
        let plugin = self.find(id)?;
        let config_json = serde_json::to_string(&config)?;
        db::upsert(&self.db, id, enabled, &config_json).await?;
        let skald = self.skald()?;
        plugin.reload(enabled, config, self.build_context(&skald)?).await?;
        self.known_state.lock().await
            .insert(id.to_string(), (enabled, config_json));
        info!(plugin = id, enabled, "plugin config updated");
        Ok(())
    }

    /// Toggle only the enabled flag, keeping existing config.
    pub async fn toggle(&self, id: &str, enabled: bool) -> Result<()> {
        let row = db::get(&self.db, id).await?
            .unwrap_or_else(|| crate::db::plugins::PluginRow {
                id:      id.to_string(),
                enabled,
                config:  "{}".to_string(),
            });
        let config: Value = serde_json::from_str(&row.config).unwrap_or(json!({}));
        self.update_config(id, enabled, config).await
    }

    // ── Background config watcher ─────────────────────────────────────────────

    /// Spawns a Tokio task that polls the DB every 30 s and calls reload()
    /// on any plugin whose (enabled, config) has changed since last check.
    /// This is the fallback path; normal updates go through update_config().
    pub fn start_config_watcher(self: &Arc<Self>, shutdown: tokio_util::sync::CancellationToken) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            interval.tick().await; // skip immediate first tick
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => { break; }
                    _ = interval.tick() => {
                        if let Err(e) = this.check_and_reload().await {
                            error!(error = %e, "plugin config watcher error");
                        }
                    }
                }
            }
        });
    }

    async fn check_and_reload(&self) -> Result<()> {
        let rows = db::list(&self.db).await?;
        let skald = self.skald()?;

        // Collect what needs reloading while holding the lock briefly.
        let to_reload: Vec<_> = {
            let known = self.known_state.lock().await;
            rows.into_iter()
                .filter(|row| {
                    known.get(&row.id)
                        .map_or(true, |(e, c)| *e != row.enabled || c != &row.config)
                })
                .collect()
        };

        for row in to_reload {
            let Ok(plugin) = self.find(&row.id) else { continue };
            let config = serde_json::from_str(&row.config).unwrap_or(json!({}));
            let ctx = self.build_context(&skald)?;
            match plugin.reload(row.enabled, config, ctx).await {
                Ok(()) => {
                    self.known_state.lock().await
                        .insert(row.id.clone(), (row.enabled, row.config));
                    info!(plugin = row.id, "plugin reloaded by config watcher");
                    if row.enabled {
                        if let Some(mem) = plugin.memory() {
                            skald.memory_manager().register(mem).await;
                        }
                    }
                }
                Err(e) => error!(plugin = row.id, error = %e, "plugin reload failed"),
            }
        }
        Ok(())
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    pub async fn list(&self) -> Result<Vec<PluginInfo>> {
        let mut out = Vec::new();
        for plugin in &self.plugins {
            let row = db::get(&self.db, plugin.id()).await?;
            let (enabled, config_json) = row
                .map(|r| (r.enabled, r.config))
                .unwrap_or((false, "{}".to_string()));
            out.push(PluginInfo {
                id:             plugin.id().to_string(),
                name:           plugin.name().to_string(),
                description:    plugin.description().to_string(),
                enabled,
                running:        plugin.is_running(),
                config:         serde_json::from_str(&config_json).unwrap_or(json!({})),
                config_schema:  plugin.config_schema(),
                runtime_status: plugin.runtime_status(),
            });
        }
        Ok(out)
    }

    /// Every registered plugin, enabled or not. Lets the core ask each one for
    /// its contributions (`Plugin::tools`, `Plugin::http_router`) without ever
    /// naming a concrete plugin type.
    pub fn all(&self) -> &[Arc<dyn Plugin>] {
        &self.plugins
    }

    pub fn get_plugin_typed<T: Plugin + 'static>(&self, id: &str) -> Option<Arc<T>> {
        self.plugins.iter()
            .find(|p| p.id() == id)
            .and_then(|p| Arc::clone(p).as_arc_any().downcast::<T>().ok())
    }

    fn find(&self, id: &str) -> Result<Arc<dyn Plugin>> {
        self.plugins.iter()
            .find(|p| p.id() == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("plugin not found: {id}"))
    }
}
