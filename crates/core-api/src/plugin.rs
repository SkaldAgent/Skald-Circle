use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::RwLock;

use crate::approval::ApprovalApi;
use crate::bus::ChatEventBus;
use crate::command::CommandApi;
use crate::system_bus::SystemEventBus;
use crate::chat_hub::ChatHubApi;
use crate::image_generate::ImageGenerateRegistry;
use crate::inbox::InboxApi;
use crate::location::LocationUpdater;
use crate::memory::Memory;
use crate::provider::ApiProviderRegistry;
use crate::remote::RemoteAccess;
use crate::secrets::SecretsApi;
use crate::transcribe::{TranscribeProvider, TranscribeRegistry};
use crate::tts::{TtsProvider, TtsRegistry};

/// Closure that builds a fresh Axum router (e.g. for the mesh-facing server).
pub type RouterFactory = Arc<dyn Fn() -> axum::Router + Send + Sync>;

/// All deps a plugin may need — passed to [`Plugin::start`] and [`Plugin::reload`].
///
/// Fields are `Arc<dyn Trait>` sourced from `core-api`.  Plugins use only the
/// fields relevant to them; unused fields are ignored.
/// `router_factory` and `remote_slot` are networking-specific — used only by
/// `RemotePlugin`.
#[derive(Clone)]
pub struct PluginContext {
    pub chat_hub:                Arc<dyn ChatHubApi>,
    /// Custom file-based slash commands (`commands/<name>/`). Read-only from the
    /// plugin side — lets the Telegram bot resolve `/command` expansions.
    pub command:                 Arc<dyn CommandApi>,
    pub approval:                Arc<dyn ApprovalApi>,
    /// Unified Inbox façade (approvals + clarifications). See plugin.md §12.2.
    pub inbox:                   Arc<dyn InboxApi>,
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
    pub event_bus:               Arc<ChatEventBus>,
    pub system_bus:              Arc<SystemEventBus>,
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

    /// Called whenever the enabled flag or config changes — including at startup.
    /// The plugin is responsible for diffing state and restarting only what changed.
    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()>;

    async fn start(&self, ctx: PluginContext) -> Result<()>;
    async fn stop(&self) -> Result<()>;

    /// Runtime state surfaced to the UI and to agents (e.g. mesh IP).
    fn runtime_status(&self) -> Option<Value> { None }

    /// Optional Axum router contributed by the plugin. When `Some`, the main
    /// `WebFrontend` nests it under `/api/plugin/<id>/` behind Skald's normal
    /// auth (plugin.md §12.3). The router must close over the plugin's own state
    /// (it receives no `State`). Default: no routes — existing plugins are
    /// unaffected.
    fn http_router(&self) -> Option<axum::Router> { None }

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

    /// Returns a [`Memory`] backend if this plugin provides one.
    fn memory(&self) -> Option<Arc<dyn Memory>> { None }

    fn as_any(&self) -> &dyn std::any::Any;
    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync>;
}
