//! Mobile connector plugin (plugin id `mobile-connector`).
//!
//! Bridges each Skald user's Inbox (approvals + clarifications + elicitations) to
//! their mobile devices over the relay, end-to-end encrypted. The **networking**
//! (v2 WS transport, E2E crypto, anti-replay counters, pairing, device
//! authorization, SQLite persistence) lives in the standalone `skald-relay-client`
//! crate; this plugin is the thin **application** layer on top of it. See
//! `data/iOS-app/v2/relay-protocol.md` for the wire contract.
//!
//! # Multi-user (blueprint §13)
//!
//! One relay identity serves many devices; each device is bound to one Skald user
//! (`auth`, admin-mediated via `mobile_bind_device`). Inbound payloads apply to
//! that user's Inbox via the [`UserChannelApi`] seam; per-user forwarders
//! (`events`) push Inbox changes only to that user's devices — never a global
//! broadcast. The HTTP reverse proxy (`proxy`) is user-agnostic: the phone renders
//! the authenticated web UI over the tunnel, which handles per-user auth itself.
//!
//! Module map:
//! - `payloads`  — E2E JSON payload schemas (inbox_update, responses, …)
//! - `auth`      — device→user bindings (config-table-backed) + config listener
//! - `app`       — `RelayApp`: per-user Inbox dispatch, bindings, the events() loop
//! - `events`    — per-user event forwarders (drive the notifiers)
//! - `notifier`  — per-user debounced Inbox pushes
//! - `proxy`     — HTTP reverse proxy to the local web UI (user-agnostic)
//! - `router`    — the QR-code HTTP endpoint
//! - `agent`     — the `RelayAgent` control trait
//! - `tools`     — `Tool` impls callable by the host (registered in the main crate)

mod agent;
mod app;
mod auth;
mod events;
mod i18n;
mod notifier;
mod payloads;
mod proxy;
mod router;
mod tools;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use core_api::plugin::{Plugin, PluginContext, PluginPage};
use skald_relay_client::{ClientState as RelayClientState, RelayClient, RelayClientConfig, SeedSource};

pub use agent::{ClientInfo, ClientState, PairingHandle, RelayAgent};
pub use tools::mobile_tools;

use app::RelayApp;

pub(crate) const PLUGIN_ID: &str = "mobile-connector";
const DEFAULT_TTL: u32 = 300;
const MAX_TTL: u32 = 600;
/// Default debounce before an unresolved Inbox item is pushed to the phone.
const DEFAULT_NOTIFY_DELAY_SECS: u64 = 20;
/// Seed file path (relative to the process working dir). Kept byte-identical to
/// the historical location so existing identities/devices survive the upgrade.
const SEED_PATH: &str = "data/relay/seed";

/// The mobile-connector plugin.
pub struct MobileConnectorPlugin {
    running: AtomicBool,
    /// Live application state — present only while running. Wrapped in `Arc` so
    /// the HTTP router (built once at startup) can dynamically point to whichever
    /// `RelayApp` is current after a reconfigure (plugin#reload → new state).
    inner: Arc<Mutex<Option<Arc<RelayApp>>>>,
    cancel: Mutex<Option<CancellationToken>>,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl MobileConnectorPlugin {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            inner: Arc::new(Mutex::new(None)),
            cancel: Mutex::new(None),
            handles: Mutex::new(Vec::new()),
        }
    }

    /// Snapshot the live app, if running. Used by the `RelayAgent` impl and the
    /// router accessor.
    async fn app(&self) -> Option<Arc<RelayApp>> {
        self.inner.lock().await.clone()
    }

    /// Start the runloop and the bus subscriber with the given config.
    async fn start_with(&self, config: Value, ctx: &PluginContext) -> Result<()> {
        let relay_url = config["relay_url"].as_str().unwrap_or("").to_string();
        if relay_url.is_empty() {
            warn!(plugin = PLUGIN_ID, "relay_url not configured; plugin idle");
        }
        let pairing_ttl = config["pairing_ttl"]
            .as_u64()
            .map(|v| (v as u32).min(MAX_TTL))
            .unwrap_or(DEFAULT_TTL);
        let require_device_confirmation = config["require_device_confirmation"]
            .as_bool()
            .unwrap_or(true);
        let notify_delay = std::time::Duration::from_secs(
            config["notify_delay_secs"]
                .as_u64()
                .unwrap_or(DEFAULT_NOTIFY_DELAY_SECS),
        );

        // Build the transport client (derives identity, inits the DB table).
        let client = Arc::new(
            RelayClient::new(
                Arc::clone(&ctx.db),
                RelayClientConfig {
                    relay_url,
                    pairing_ttl,
                    seed: SeedSource::Path(SEED_PATH.into()),
                },
            )
            .await?,
        );
        info!(
            plugin = PLUGIN_ID,
            namespace = client.namespace_id_hex(),
            "mobile-connector identity loaded"
        );
        client.start().await?;

        let cancel = CancellationToken::new();

        // Load device→user bindings from the config table (or default if absent).
        let bindings = auth::load_config(&*ctx.config).await.unwrap_or_default();
        info!(plugin = PLUGIN_ID, bindings = bindings.bindings.len(), "bindings loaded");

        let app = RelayApp::new(
            Arc::clone(&client),
            Arc::clone(&ctx.user_channel),
            Arc::clone(&ctx.config),
            Arc::clone(&ctx.i18n),
            bindings,
            require_device_confirmation,
            notify_delay,
            cancel.clone(),
        );

        let mut handles = Vec::new();

        // Event loop: apply inbound payloads + pairing authorization policy, and
        // lazily spawn per-user forwarders when a bound device becomes active.
        {
            let app2 = Arc::clone(&app);
            let rx = client.events();
            handles.push(tokio::spawn(async move {
                app2.run_event_loop(rx).await;
            }));
        }

        // Config listener: reloads bindings when the "mobile-connector" config key
        // changes (e.g. the bind tool writes a new binding) and (re)spawns forwarders.
        {
            let app3 = Arc::clone(&app);
            let bus_rx = ctx.system_bus.subscribe();
            handles.push(tokio::spawn(auth::config_listener(app3, bus_rx)));
        }

        // Reconcile loop: (re)spawns forwarders for bound users as they unlock. Its
        // first tick fires immediately (covering already-unlocked users at start),
        // then it periodically catches users who log in later — there is no "user
        // unlocked" event to hook, and at boot every pool is locked (§9).
        {
            let app4 = Arc::clone(&app);
            handles.push(tokio::spawn(events::reconcile_loop(app4)));
        }

        // HTTP reverse proxy: bridge `http-local-proxy` pipes to the local web
        // server so the native app can render the web UI over the relay (no NAT
        // hole / Tailscale). Pinned to 127.0.0.1:<web_port>; access gated by the
        // relay's pipe auth (only authorized namespace members).
        {
            let client2 = Arc::clone(&client);
            let c       = cancel.clone();
            let port    = ctx.web_port;
            handles.push(tokio::spawn(async move {
                crate::proxy::run_proxy_loop(client2, port, c).await;
            }));
        }

        *self.inner.lock().await = Some(app);
        *self.cancel.lock().await = Some(cancel);
        *self.handles.lock().await = handles;
        self.running.store(true, Ordering::Relaxed);
        info!(plugin = PLUGIN_ID, "mobile-connector started");
        Ok(())
    }

    async fn stop_inner(&self) {
        if let Some(c) = self.cancel.lock().await.take() {
            c.cancel();
        }
        // Shut down the transport (cancels + joins the WS loop) before dropping
        // the app, cancelling any armed (not-yet-fired) per-user push timers first.
        if let Some(app) = self.inner.lock().await.take() {
            for notifier in app.notifiers.lock().await.values() {
                notifier.cancel_all().await;
            }
            app.client().shutdown().await;
        }
        for h in self.handles.lock().await.drain(..) {
            let _ = h.await;
        }
        self.running.store(false, Ordering::Relaxed);
        info!(plugin = PLUGIN_ID, "mobile-connector stopped");
    }
}

impl Default for MobileConnectorPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Plugin for MobileConnectorPlugin {
    fn id(&self) -> &str { PLUGIN_ID }
    fn name(&self) -> &str { "Mobile Connector" }
    fn description(&self) -> &str {
        "Connects mobile apps to this Skald instance via the relay: bridges the \
         Inbox (approvals + clarifications) to phones with end-to-end encryption."
    }
    fn is_running(&self) -> bool { self.running.load(Ordering::Relaxed) }

    /// Access is the device→user binding (§13), not a `plugin_access` grant, so
    /// the admin Plugins UI hides the "User access" checklist for this plugin.
    fn manages_own_access(&self) -> bool { true }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "relay_url": {
                    "type": "string",
                    "title": "Relay URL",
                    "description": "wss:// URL of the relay (e.g. wss://relay.skaldagent.net/v1/ws).",
                },
                "pairing_ttl": {
                    "type": "integer",
                    "default": DEFAULT_TTL,
                    "maximum": MAX_TTL,
                    "title": "Pairing TTL (seconds)",
                    "description": "How long a pairing window stays open. Max 600.",
                },
                "require_device_confirmation": {
                    "type": "boolean",
                    "default": true,
                    "title": "Require device confirmation",
                    "description": "Require manual confirmation before a newly paired device is authorized (recommended).",
                },
                "notify_delay_secs": {
                    "type": "integer",
                    "default": DEFAULT_NOTIFY_DELAY_SECS,
                    "minimum": 0,
                    "title": "Notification delay (seconds)",
                    "description": "Wait this long before pushing an approval/clarification to the phone. If you answer on the computer within the window, no phone notification is sent. Set 0 to push immediately. (MCP elicitations are Inbox-only and always pushed immediately, regardless of this setting.)",
                }
            }
        })
    }

    fn runtime_status(&self) -> Option<Value> {
        if !self.running.load(Ordering::Relaxed) {
            return None;
        }
        // Synchronous status: report connection flag from the live client.
        let connected = self
            .inner
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|app| app.client().is_connected()))
            .unwrap_or(false);
        Some(json!({ "connected": connected }))
    }

    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()> {
        match (enabled, self.is_running()) {
            (true, false) => self.start_with(config, &ctx).await,
            (false, true) => { self.stop_inner().await; Ok(()) }
            (true, true) => { self.stop_inner().await; self.start_with(config, &ctx).await }
            (false, false) => Ok(()),
        }
    }

    async fn start(&self, _ctx: PluginContext) -> Result<()> {
        // Lifecycle is driven by reload(enabled, ...); nothing to do here.
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.stop_inner().await;
        Ok(())
    }

    fn http_router(&self) -> Option<axum::Router> {
        // The router is collected once at WebFrontend startup, but the inner
        // `RelayApp` may be replaced on reconfigure.  We hand over the shared
        // `Arc<Mutex<…>>` so the QR route always resolves the *current* app.
        Some(router::build(Arc::clone(&self.inner)))
    }

    /// Two admin-only console pages served from this plugin's own router
    /// (`web/*.js`). `manages_own_access` already hides them from non-admins.
    fn web_pages(&self) -> Vec<PluginPage> {
        vec![
            PluginPage {
                page_id:    "pairing",
                title:      "Pair a device".into(),
                icon:       "qr-code",
                entry:      "web/pairing.js".into(),
                admin_only: true,
                priority:   10,
            },
            PluginPage {
                page_id:    "devices",
                title:      "Mobile devices".into(),
                icon:       "phone",
                entry:      "web/devices.js".into(),
                admin_only: true,
                priority:   20,
            },
        ]
    }

    /// Control tools (plugin.md §11). They close over the plugin itself as a
    /// `RelayAgent` and call into it lazily, so building them before the runloop
    /// starts is fine — they fail gracefully while it is stopped.
    fn tools(self: Arc<Self>) -> Vec<Arc<dyn core_api::tool::Tool>> {
        crate::tools::mobile_tools(self)
    }

    /// Backend translation tables — the router's error/response strings,
    /// namespaced `plugin.mobile-connector.*`. See `crate::i18n`.
    fn i18n(&self) -> Vec<core_api::i18n::LocaleBundle> {
        crate::i18n::bundles()
    }

    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> { self }
}

// ── RelayAgent control surface ─────────────────────────────────────────────────

#[async_trait]
impl RelayAgent for MobileConnectorPlugin {
    async fn start_pairing(&self, ttl_secs: u32) -> Result<PairingHandle> {
        let app = self.app().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        let ttl = if ttl_secs == 0 { 0 } else { ttl_secs.min(MAX_TTL) };
        let started = app.client().start_pairing(ttl).await?;
        Ok(PairingHandle {
            url: format!("/api/plugin/{PLUGIN_ID}/pairingqrcode?code={}", started.code),
            code: started.code,
            expires_at: started.expires_at,
        })
    }

    async fn stop_pairing(&self) -> Result<()> {
        let app = self.app().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        app.client().stop_pairing().await
    }

    fn agent_ed25519_pub(&self) -> [u8; 32] {
        self.inner
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|app| app.client().agent_ed25519_pub()))
            .unwrap_or([0u8; 32])
    }

    fn namespace_id(&self) -> String {
        self.inner
            .try_lock()
            .ok()
            .and_then(|g| g.as_ref().map(|app| app.client().namespace_id_hex()))
            .unwrap_or_default()
    }

    async fn list_clients(&self) -> Vec<ClientInfo> {
        let Some(app) = self.app().await else { return Vec::new() };
        let rows = app.client().list_clients().await;
        let bindings = app.bindings.read().await;
        rows.into_iter()
            .map(|r| {
                let bound_user = bindings.user_for_pubkey(&hex::encode(r.ed25519_pub));
                ClientInfo {
                    ed25519_pub: r.ed25519_pub,
                    x25519_pub: r.x25519_pub,
                    state: match r.state {
                        RelayClientState::Authorized => ClientState::Authorized,
                        RelayClientState::Pending => ClientState::Pending,
                    },
                    device_info: r.device_info,
                    platform: r.platform,
                    last_seen: r.last_seen,
                    bound_user,
                }
            })
            .collect()
    }

    async fn bind_device(
        &self,
        ed25519_pub: [u8; 32],
        user_id: String,
        display: Option<String>,
    ) -> Result<()> {
        let app = self.app().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        app.bind_device(ed25519_pub, user_id, display).await
    }

    async fn revoke_client(&self, ed25519_pub: [u8; 32]) -> Result<()> {
        let app = self.app().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        app.revoke_device(ed25519_pub).await
    }
}
