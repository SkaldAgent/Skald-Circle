// The manager only ever handles `Arc<dyn Plugin>`. Naming a concrete plugin crate
// here would make the core depend on every plugin — and, through
// `plugin-transcribe-whisper-local`, on a C build — for no gain: the consumer
// constructs the plugin list and passes it to `Skald::new`.
pub use core_api::plugin::{Plugin, PluginContext, PluginPage, RouterFactory};

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

const PLUGIN_START_TIMEOUT_SECS: u64 = 30;
const PLUGIN_STOP_TIMEOUT_SECS:  u64 = 5;

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{error, info, warn};

use crate::db::{plugin_access, plugin_user_configs, plugins as db};
use crate::skald::Skald;

// ── Public plugin info (returned by list_items tool and REST API) ─────────────

#[derive(Debug, Clone, Serialize)]
pub struct PluginInfo {
    pub id:                 String,
    pub name:               String,
    pub description:        String,
    pub enabled:            bool,
    pub running:            bool,
    pub config:             Value,
    pub config_schema:      Value,
    /// Whether the plugin contributes an `http_router()` — its routes are
    /// mounted at boot and gated at runtime, so they serve as soon as the
    /// plugin is enabled (no restart).
    pub has_router:         bool,
    /// Whether the plugin gates access through its own binding lifecycle — the
    /// admin UI hides the "User access" checklist when true (see the trait).
    pub manages_own_access: bool,
    /// Whether the plugin contributes a user-facing (`!admin_only`) page via
    /// `web_pages()` — the static signal that it has per-user settings of its
    /// own (e.g. Telegram's pairing page). Informational, for the admin UI.
    pub has_user_page:      bool,
    /// Whether the plugin-detail page shows the generic `config_schema` form
    /// (`false` = the plugin hosts its own config UI in one of its pages).
    pub config_in_detail_page: bool,
    pub runtime_status:     Option<Value>,
}

/// One user's view of a plugin they may use — served by `GET /api/plugins/mine`.
/// Read by the plugin's own page fragment (e.g. Telegram's pairing page reads
/// its `{linked, chat_id}` status blob from `user_config`).
#[derive(Debug, Clone, Serialize)]
pub struct UserPluginView {
    pub id:          String,
    pub name:        String,
    pub description: String,
    pub user_config: Value,
}

/// A plugin-contributed web page as seen by one user — served by
/// `GET /api/plugins/pages`. `entry_url` is already resolved against the
/// plugin's router mount, so the frontend can `import()` it directly.
#[derive(Debug, Clone, Serialize)]
pub struct PluginPageInfo {
    pub plugin_id:   String,
    pub page_id:     String,
    pub title:       String,
    pub icon:        String,
    pub priority:    i32,
    pub entry_url:   String,
    /// Mirrors [`core_api::plugin::PluginPage::admin_only`]. Lets the admin
    /// Plugins UI recognise a plugin's own config page and defer to it (hide the
    /// generic `config_schema` form, link out instead).
    pub admin_only:  bool,
    /// Fragment-contract version the host speaks. Always 1 for now — bump when
    /// the contract changes so old hosts can refuse new fragments cleanly.
    pub api_version: u32,
}

// ── Per-user config store (the PluginUserConfigApi injected into PluginContext) ─

/// `PluginUserConfigApi` over the system pool. Admin-readable by design —
/// see `db::plugin_user_configs`.
struct UserConfigStore {
    db: Arc<SqlitePool>,
}

#[async_trait]
impl core_api::user_plugin_config::PluginUserConfigApi for UserConfigStore {
    async fn get(&self, plugin_id: &str, user_id: &str) -> Result<Option<Value>> {
        plugin_user_configs::get(&self.db, plugin_id, user_id).await
    }

    async fn set(&self, plugin_id: &str, user_id: &str, config: Value) -> Result<()> {
        plugin_user_configs::set(&self.db, plugin_id, user_id, &config).await
    }

    async fn delete(&self, plugin_id: &str, user_id: &str) -> Result<()> {
        plugin_user_configs::delete(&self.db, plugin_id, user_id).await
    }
}

// ── PluginManager ─────────────────────────────────────────────────────────────

pub struct PluginManager {
    plugins:        Vec<Arc<dyn Plugin>>,
    db:             Arc<SqlitePool>,
    user_config:    Arc<UserConfigStore>,
    skald:          OnceLock<Arc<Skald>>,
    /// Provided by WebFrontend before start_enabled() is called.
    router_factory: OnceLock<RouterFactory>,
    /// HTTP port the web server is bound to — provided by WebFrontend before start_enabled().
    web_port:       OnceLock<u16>,
    /// Backend i18n catalog, built once from every plugin's `Plugin::i18n()` on
    /// first context build (all plugins are registered by then). Injected into
    /// every `PluginContext` so a plugin can localize its own backend strings.
    i18n:           OnceLock<Arc<crate::i18n::I18nCatalog>>,
    /// Last known (enabled, config_json) per plugin id — used by the watcher.
    known_state:    Mutex<HashMap<String, (bool, String)>>,
}

impl PluginManager {
    pub fn new(db: Arc<SqlitePool>) -> Self {
        Self {
            plugins:        Vec::new(),
            user_config:    Arc::new(UserConfigStore { db: Arc::clone(&db) }),
            db,
            skald:          OnceLock::new(),
            router_factory: OnceLock::new(),
            web_port:       OnceLock::new(),
            i18n:           OnceLock::new(),
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

    /// The shared backend i18n catalog, built once by merging every registered
    /// plugin's `Plugin::i18n()` bundles. All plugins are registered before the
    /// first `build_context`, so a single lazy build is correct.
    fn i18n(&self) -> Arc<dyn core_api::i18n::I18nApi> {
        let catalog = self.i18n.get_or_init(|| {
            let mut bundles = Vec::new();
            for plugin in &self.plugins {
                bundles.extend(plugin.i18n());
            }
            Arc::new(crate::i18n::I18nCatalog::new(Arc::clone(&self.db), bundles))
        });
        Arc::clone(catalog) as Arc<dyn core_api::i18n::I18nApi>
    }

    fn build_context(&self, skald: &Skald) -> Result<PluginContext> {
        let router_factory = self.router_factory.get().cloned()
            .ok_or_else(|| anyhow::anyhow!("PluginManager: router_factory not set"))?;
        let web_port = self.web_port.get().copied()
            .ok_or_else(|| anyhow::anyhow!("PluginManager: web_port not set"))?;

        Ok(PluginContext {
            command:                 Arc::clone(skald.command_manager()) as _,
            config:                  Arc::clone(skald.config()) as Arc<dyn core_api::config_api::ConfigApi>,
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
            chat_bus:                Arc::clone(skald.event_bus()),
            user_channel:            self.skald()? as Arc<dyn core_api::user_channel::UserChannelApi>,
            user_config:             Arc::clone(&self.user_config) as _,
            i18n:                    self.i18n(),
            web_port,
            remote_slot:             Arc::clone(skald.remote()),
            router_factory,
        })
    }

    /// Collects the HTTP routers contributed by **every** registered plugin —
    /// enabled or not. Returns `(plugin_id, router)` pairs; the caller
    /// (`WebFrontend::start`) nests each under `/api/plugin/<id>/` behind the
    /// auth + enabled gates, so a disabled plugin's routes answer 404 and
    /// enabling one at runtime serves them immediately (no restart).
    ///
    /// Call this AFTER `start_enabled()` so a started plugin's router can close
    /// over state initialised during `reload`/`start`. The router must still be
    /// safe to build for a plugin that never started (see `Plugin::http_router`).
    pub async fn collect_plugin_routers(&self) -> Vec<(String, axum::Router)> {
        let mut out = Vec::new();
        for plugin in &self.plugins {
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
                id:                 plugin.id().to_string(),
                name:               plugin.name().to_string(),
                description:        plugin.description().to_string(),
                enabled,
                running:            plugin.is_running(),
                config:             serde_json::from_str(&config_json).unwrap_or(json!({})),
                config_schema:      plugin.config_schema(),
                has_router:         plugin.http_router().is_some(),
                manages_own_access: plugin.manages_own_access(),
                has_user_page:      plugin.web_pages().iter().any(|pg| !pg.admin_only),
                config_in_detail_page: plugin.config_in_detail_page(),
                runtime_status:     plugin.runtime_status(),
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

    // ── Per-user access & configuration ───────────────────────────────────────

    /// The plugins a user may interact with: **enabled** and granted in
    /// `plugin_access` (admins see every enabled plugin). Each entry carries
    /// the user's current config blob — read by the plugin's own page
    /// fragment, never rendered by a generic core UI.
    pub async fn list_accessible(&self, user_id: &str, is_admin: bool) -> Result<Vec<UserPluginView>> {
        let granted: std::collections::HashSet<String> = if is_admin {
            std::collections::HashSet::new()
        } else {
            plugin_access::plugin_ids_for_user(&self.db, user_id).await?.into_iter().collect()
        };
        let mut out = Vec::new();
        for plugin in &self.plugins {
            // Binding-managed plugins (e.g. mobile-connector) own their access
            // model — there is no per-user config blob of ours to show them.
            if plugin.manages_own_access() {
                continue;
            }
            let enabled = db::get(&self.db, plugin.id()).await?
                .map(|r| r.enabled)
                .unwrap_or(false);
            if !enabled || (!is_admin && !granted.contains(plugin.id())) {
                continue;
            }
            let user_config = plugin_user_configs::get(&self.db, plugin.id(), user_id)
                .await?
                .unwrap_or(json!({}));
            out.push(UserPluginView {
                id:          plugin.id().to_string(),
                name:        plugin.name().to_string(),
                description: plugin.description().to_string(),
                user_config,
            });
        }
        Ok(out)
    }

    pub async fn has_access(&self, id: &str, user_id: &str) -> Result<bool> {
        plugin_access::has_access(&self.db, id, user_id).await
    }

    /// The web pages a user sees in the frontend menu: every `web_pages()`
    /// entry of every **enabled** plugin, filtered by audience — `admin_only`
    /// pages go to the admin role only; the others require the `plugin_access`
    /// grant (admins see all). Binding-managed plugins (`manages_own_access`)
    /// own their access model (e.g. the device↔user binding), so their
    /// non-`admin_only` pages are visible to every logged-in user and the page
    /// itself scopes what each caller sees.
    pub async fn web_pages_for(&self, user_id: &str, is_admin: bool) -> Result<Vec<PluginPageInfo>> {
        let granted: std::collections::HashSet<String> = if is_admin {
            std::collections::HashSet::new()
        } else {
            plugin_access::plugin_ids_for_user(&self.db, user_id).await?.into_iter().collect()
        };
        let mut out = Vec::new();
        for plugin in &self.plugins {
            let pages = plugin.web_pages();
            if pages.is_empty() {
                continue;
            }
            if !self.is_enabled(plugin.id()).await? {
                continue;
            }
            let owns_access = plugin.manages_own_access();
            for page in pages {
                let visible = if is_admin {
                    true
                } else if page.admin_only {
                    false
                } else if owns_access {
                    true
                } else {
                    granted.contains(plugin.id())
                };
                if visible {
                    out.push(PluginPageInfo {
                        plugin_id:   plugin.id().to_string(),
                        page_id:     page.page_id.to_string(),
                        title:       page.title,
                        icon:        page.icon.to_string(),
                        priority:    page.priority,
                        entry_url:   format!("/api/plugin/{}/{}", plugin.id(), page.entry),
                        admin_only:  page.admin_only,
                        api_version: 1,
                    });
                }
            }
        }
        out.sort_by_key(|p| p.priority);
        Ok(out)
    }

    pub async fn is_enabled(&self, id: &str) -> Result<bool> {
        Ok(db::get(&self.db, id).await?.map(|r| r.enabled).unwrap_or(false))
    }

    /// The user ids granted access to a plugin (admin UI checklist).
    pub async fn list_grants(&self, id: &str) -> Result<Vec<String>> {
        self.find(id)?;
        plugin_access::users_for_plugin(&self.db, id).await
    }

    pub async fn set_grants(&self, id: &str, user_ids: &[String]) -> Result<()> {
        self.find(id)?;
        plugin_access::set_access(&self.db, id, user_ids).await
    }

    /// Applies a user's per-plugin config submission, received from the
    /// plugin's own page fragment. The plugin must be enabled and the caller
    /// must hold access (enforced by the API layer); what the submission means
    /// is entirely the plugin's business (see `Plugin::update_user_config`).
    pub async fn update_user_config(&self, id: &str, user_id: &str, config: Value) -> Result<()> {
        let plugin = self.find(id)?;
        if !self.is_enabled(id).await? {
            anyhow::bail!("plugin is not enabled: {id}");
        }
        let skald = self.skald()?;
        plugin.update_user_config(user_id, config, &self.build_context(&skald)?).await
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

#[cfg(test)]
mod tests {
    use super::*;
    use core_api::plugin::PluginPage;

    struct FakePlugin {
        id:          &'static str,
        pages:       Vec<PluginPage>,
        owns_access: bool,
    }

    #[async_trait]
    impl Plugin for FakePlugin {
        fn id(&self) -> &str { self.id }
        fn name(&self) -> &str { self.id }
        fn description(&self) -> &str { "" }
        fn is_running(&self) -> bool { false }
        fn manages_own_access(&self) -> bool { self.owns_access }
        fn web_pages(&self) -> Vec<PluginPage> { self.pages.clone() }
        async fn reload(&self, _enabled: bool, _config: Value, _ctx: PluginContext) -> Result<()> { Ok(()) }
        async fn start(&self, _ctx: PluginContext) -> Result<()> { Ok(()) }
        async fn stop(&self) -> Result<()> { Ok(()) }
        fn as_any(&self) -> &dyn std::any::Any { self }
        fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> { self }
    }

    fn page(page_id: &'static str, admin_only: bool, priority: i32) -> PluginPage {
        PluginPage {
            page_id,
            title: page_id.to_string(),
            icon: "puzzle",
            entry: format!("web/{page_id}.js"),
            admin_only,
            priority,
        }
    }

    async fn test_manager(tag: &str) -> PluginManager {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir()
            .join(format!("skald-plugin-test-{tag}-{}-{nanos}", std::process::id()))
            .join("system.db");
        let pool = crate::db::init_system_pool(path.to_str().unwrap()).await.unwrap();
        PluginManager::new(Arc::new(pool))
    }

    #[tokio::test]
    async fn web_pages_for_filters_by_enabled_audience_and_access() {
        let mut mgr = test_manager("pages-filter").await;
        mgr.register_arc(Arc::new(FakePlugin {
            id: "alpha",
            pages: vec![page("admin-console", true, 10), page("user-dash", false, 20)],
            owns_access: false,
        }));
        mgr.register_arc(Arc::new(FakePlugin {
            id: "beta",
            pages: vec![page("off-page", false, 5)],
            owns_access: false,
        }));
        mgr.register_arc(Arc::new(FakePlugin {
            id: "gamma",
            pages: vec![page("pairing", false, 15)],
            owns_access: true,
        }));
        // alpha + gamma enabled, beta disabled; user u1 holds grants on both.
        db::upsert(&mgr.db, "alpha", true, "{}").await.unwrap();
        db::upsert(&mgr.db, "beta", false, "{}").await.unwrap();
        db::upsert(&mgr.db, "gamma", true, "{}").await.unwrap();
        for (id, username) in [("u1", "user-one"), ("u2", "user-two")] {
            sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES (?, ?, 'admin', 0)")
                .bind(id).bind(username).execute(&*mgr.db).await.unwrap();
        }
        plugin_access::grant(&mgr.db, "alpha", "u1").await.unwrap();
        plugin_access::grant(&mgr.db, "gamma", "u1").await.unwrap();

        // Admin: everything enabled, priority-ascending.
        let admin = mgr.web_pages_for("admin-user", true).await.unwrap();
        let got: Vec<(&str, &str)> = admin.iter()
            .map(|p| (p.plugin_id.as_str(), p.page_id.as_str())).collect();
        assert_eq!(got, vec![
            ("alpha", "admin-console"),
            ("gamma", "pairing"),
            ("alpha", "user-dash"),
        ]);
        assert_eq!(admin[0].entry_url, "/api/plugin/alpha/web/admin-console.js");
        assert_eq!(admin[0].api_version, 1);

        // Non-admin: only the non-admin_only page of a granted, enabled,
        // non-binding-managed plugin — beta is disabled, alpha's admin console
        // is admin_only. gamma manages its own access, so its page is visible
        // to everyone and self-scopes per caller.
        let user = mgr.web_pages_for("u1", false).await.unwrap();
        let got: Vec<(&str, &str)> = user.iter()
            .map(|p| (p.plugin_id.as_str(), p.page_id.as_str())).collect();
        assert_eq!(got, vec![("gamma", "pairing"), ("alpha", "user-dash")]);

        // A user with no grants sees only the binding-managed page.
        let stranger = mgr.web_pages_for("u2", false).await.unwrap();
        let got: Vec<(&str, &str)> = stranger.iter()
            .map(|p| (p.plugin_id.as_str(), p.page_id.as_str())).collect();
        assert_eq!(got, vec![("gamma", "pairing")]);
    }
}
