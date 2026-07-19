use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::command::CommandApi;
use crate::config_api::ConfigApi;
use crate::i18n::I18nApi;
use crate::system_bus::SystemEventBus;
use crate::image_generate::ImageGenerateRegistry;
use crate::location::LocationUpdater;
use crate::memory::Memory;
use crate::provider::ApiProviderRegistry;
use crate::remote::RemoteAccess;
use crate::secrets::SecretsApi;
use crate::transcribe::{TranscribeProvider, TranscribeRegistry};
use crate::tts::{TtsProvider, TtsRegistry};
use crate::user_channel::UserChannelApi;
use crate::user_plugin_config::PluginUserConfigApi;

/// Closure that builds a fresh Axum router (e.g. for the mesh-facing server).
pub type RouterFactory = Arc<dyn Fn() -> axum::Router + Send + Sync>;

/// The authenticated caller behind a plugin-router request.
///
/// The frontend's auth layer injects this into request extensions for every
/// gated request (alongside its own richer, bin-private `AuthUser`). A plugin
/// router cannot name bin-crate types, so this is how a plugin handler learns
/// *who* is calling — e.g. to bind a freshly paired device to the admin who
/// opened the pairing window. Gate admin-only actions with
/// [`crate::user_channel::UserChannelApi::plugin_access`] (which returns `true`
/// only for admins when the plugin `manages_own_access`).
#[derive(Clone, Debug)]
pub struct Caller {
    pub user_id: String,
}

/// A web UI page contributed by a plugin — see [`Plugin::web_pages`].
#[derive(Debug, Clone)]
pub struct PluginPage {
    /// Stable id, unique within the plugin — used in the route
    /// (`#plugin/<plugin_id>/<page_id>`). e.g. "pairing", "devices".
    pub page_id:    &'static str,
    /// Menu label. Shown as-is (the plugin owns its UI strings).
    pub title:      String,
    /// Bootstrap Icons name (e.g. "qr-code", "phone"), rendered as `bi-<icon>`.
    pub icon:       &'static str,
    /// Path of the page's ES module **inside this plugin's router**, e.g.
    /// "web/pairing.js" — served at `/api/plugin/<id>/web/pairing.js`.
    pub entry:      String,
    /// `true` = only the built-in admin role sees the menu entry (e.g. a
    /// pairing/devices console). `false` = any user with `plugin_access`.
    pub admin_only: bool,
    /// Menu ordering — ascending; the native menu will adopt the same field
    /// when it is reworked. Use round numbers (10, 20, …) to leave room.
    pub priority:   i32,
}

/// All deps a plugin may need — passed to [`Plugin::start`] and [`Plugin::reload`].
///
/// Fields are `Arc<dyn Trait>` sourced from `core-api`.  Plugins use only the
/// fields relevant to them; unused fields are ignored.
/// `router_factory` and `remote_slot` are networking-specific — used only by
/// `RemotePlugin`.
#[derive(Clone)]
pub struct PluginContext {
    /// Custom file-based slash commands (`commands/<name>/`). Read-only from the
    /// plugin side — lets the Telegram bot resolve `/command` expansions.
    pub command:                 Arc<dyn CommandApi>,
    /// Key/value config store (`config` table in `system.db`). `set` emits
    /// `ConfigKeyUpdated` on the system bus.
    pub config:                  Arc<dyn ConfigApi>,
    /// Skald's shared SQLite pool — lets plugins create/use their own tables
    /// (e.g. `relay_*`) in the main DB. See plugin.md §12.1.
    pub db:                      Arc<sqlx::SqlitePool>,
    pub secrets:                 Arc<dyn SecretsApi>,
    pub transcribe:              Arc<dyn TranscribeProvider>,
    pub transcribe_registry:     Arc<dyn TranscribeRegistry>,
    pub image_generate_registry: Arc<dyn ImageGenerateRegistry>,
    pub tts_registry:            Arc<dyn TtsRegistry>,
    pub tts_provider:            Arc<dyn TtsProvider>,
    pub api_provider_registry:   Arc<dyn ApiProviderRegistry>,
    pub location:                Arc<dyn LocationUpdater>,
    pub system_bus:              Arc<SystemEventBus>,
    /// Channel-to-session resolver (blueprint §13). Lets channel plugins
    /// (Telegram, mobile, …) look up an unlocked user's chat hub, approval
    /// manager and event stream by user id.
    pub user_channel:            Arc<dyn UserChannelApi>,
    /// Per-user plugin configuration store (`plugin_user_configs` table).
    /// Admin-readable — never secrets.
    pub user_config:             Arc<dyn PluginUserConfigApi>,
    /// Backend localization. Turns a plugin's namespaced string key into text in
    /// the caller's language (`i18n.for_user(user_id, key, args)`). The catalog
    /// is built at boot from every plugin's [`Plugin::i18n`]. See `core_api::i18n`.
    pub i18n:                    Arc<dyn I18nApi>,
    pub web_port:                u16,
    pub remote_slot:             Arc<RwLock<Option<Arc<dyn RemoteAccess>>>>,
    pub router_factory:          RouterFactory,
}

/// Plugin lifecycle contract.
///
/// Each plugin implements this trait. The `PluginManager` in the main crate
/// manages their lifecycle and passes a `PluginContext` on every start/reload.
#[async_trait]
pub trait Plugin: Send + Sync {
    fn id(&self)          -> &str;
    fn name(&self)        -> &str;
    fn description(&self) -> &str;
    fn is_running(&self)  -> bool;

    /// JSON Schema describing the plugin's config fields.
    fn config_schema(&self) -> Value { serde_json::json!({}) }

    /// JSON Schema describing the plugin's *per-user* config fields (e.g.
    /// Telegram's pairing code). Empty schema (the default) = the plugin has
    /// no per-user settings and does not appear as configurable in the user
    /// UI. Values are stored admin-readable in `system.db` — never secrets.
    fn user_config_schema(&self) -> Value { serde_json::json!({}) }

    /// Applies a per-user config submission. The default just stores the blob
    /// in the generic store; plugins that need validation or a side effect
    /// (e.g. Telegram turning a pairing code into a chat binding) override it
    /// and may store a sanitized status blob for the UI via `ctx.user_config`.
    async fn update_user_config(&self, user_id: &str, config: Value, ctx: &PluginContext) -> Result<()> {
        ctx.user_config.set(self.id(), user_id, config).await
    }

    /// Whether the plugin decides *who may use it* through its own binding /
    /// pairing lifecycle rather than the generic `plugin_access` grants — e.g.
    /// the mobile connector, whose access is the admin-mediated device→user
    /// binding (§13). When `true`, the admin Plugins UI suppresses the "User
    /// access" checklist (it would control nothing) and the plugin never appears
    /// in a user's "My plugins" view. Default `false`: access is the admin's
    /// per-user `plugin_access` grant (as Telegram uses — its grant gates the
    /// bot at runtime even though pairing is self-service).
    fn manages_own_access(&self) -> bool { false }

    /// Called whenever the enabled flag or config changes — including at startup.
    /// The plugin is responsible for diffing state and restarting only what changed.
    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()>;

    async fn start(&self, ctx: PluginContext) -> Result<()>;
    async fn stop(&self) -> Result<()>;

    /// Runtime state surfaced to the UI and to agents (e.g. mesh IP).
    fn runtime_status(&self) -> Option<Value> { None }

    /// Optional Axum router contributed by the plugin. When `Some`, the main
    /// `WebFrontend` nests it under `/api/plugin/<id>/` behind Skald's normal
    /// auth plus a runtime enabled-gate: **every** plugin router is mounted at
    /// boot, and a disabled plugin's routes answer 404 until it is enabled
    /// (no restart needed).
    ///
    /// Contract:
    /// - Building the router must be cheap and safe even if the plugin never
    ///   starts — it is called at boot regardless of the enabled flag. Handlers
    ///   must tolerate the not-running state (an enabled-but-crashed plugin can
    ///   still receive requests). Resolve runtime state per request through a
    ///   shared cell (e.g. `Arc<Mutex<Option<State>>>`) rather than capturing it.
    /// - The router closes over the plugin's own state (it receives no `State`).
    /// - Page fragments and other assets are served from here too; responses
    ///   automatically get `Cache-Control: no-cache` from the shell.
    ///
    /// Default: no routes — existing plugins are unaffected.
    fn http_router(&self) -> Option<axum::Router> { None }

    /// Web UI pages this plugin contributes to the frontend, surfaced as menu
    /// entries and served to the browser by `GET /api/plugins/pages`.
    ///
    /// Each page is a self-contained ES module served by this plugin's own
    /// [`Plugin::http_router`] at `entry` (e.g. `web/pairing.js` →
    /// `/api/plugin/<id>/web/pairing.js`). Fragment contract:
    /// - default-export an `HTMLElement` class (a Lit element works); the host
    ///   registers it as a custom element and sets the `plugin-id` attribute;
    /// - the fragment talks to its own backend only through
    ///   `/api/plugin/<id>/…` — no host APIs are injected;
    /// - it runs with the full privileges of the logged-in session (plugins are
    ///   trusted — they ship in the binary);
    /// - it localizes by shipping its own `{en,it,fr}` string table and
    ///   registering it via `addStrings` into the host's shared `i18n.js`, then
    ///   using the same `t()`/`I18nMixin` (keys namespaced `plugin.<id>.`).
    ///
    /// Default: no pages.
    fn web_pages(&self) -> Vec<PluginPage> { Vec::new() }

    /// Tools this plugin contributes to the registry — the sibling of
    /// [`Plugin::http_router`].
    ///
    /// The receiver is `Arc<Self>` because the tools a plugin builds usually
    /// call back into it, so it must hand them its own handle. Without this
    /// hook the core has to name concrete plugin crates in order to downcast
    /// them, and ends up depending on every plugin in the tree.
    ///
    /// Called once while the tool registry is built, *before* the plugin's
    /// runloop starts: the tools must tolerate being invoked while their plugin
    /// is stopped. Default: no tools.
    fn tools(self: Arc<Self>) -> Vec<Arc<dyn crate::tool::Tool>> { Vec::new() }

    /// Backend translation tables this plugin contributes — one
    /// [`crate::i18n::LocaleBundle`] per locale it ships. Collected once at boot
    /// into the shared catalog behind [`PluginContext::i18n`]. Keys must be
    /// namespaced (`plugin.<id>.<key>`). Default: no strings (plugin emits no
    /// localized backend text). See `core_api::i18n`.
    fn i18n(&self) -> Vec<crate::i18n::LocaleBundle> { Vec::new() }

    /// Returns a [`Memory`] backend if this plugin provides one.
    fn memory(&self) -> Option<Arc<dyn Memory>> { None }

    fn as_any(&self) -> &dyn std::any::Any;
    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync>;
}
