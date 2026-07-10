//! Mobile connector plugin (plugin id `mobile-connector`).
//!
//! Bridges Skald's Inbox (approvals + clarifications) to mobile apps over the
//! relay. The **networking** (v2 WS transport, E2E crypto, anti-replay counters,
//! pairing, device authorization, SQLite persistence) lives in the standalone
//! `skald-relay-client` crate; this plugin is the thin **application** layer on
//! top of it. See `data/iOS-app/v2/relay-protocol.md` for the wire contract and
//! `docs/relay/` for the client/server split.
//!
//! Module map:
//! - `payloads`  — E2E JSON payload schemas (inbox_update, responses, …)
//! - `app`       — `RelayApp`: Inbox dispatch, auth policy, the events() loop
//! - `router`    — the QR-code HTTP endpoint
//! - `agent`     — the `RelayAgent` control trait
//! - `tools`     — `Tool` impls callable by the host (registered in the main crate)

mod agent;
mod app;
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

use core_api::plugin::{Plugin, PluginContext};
use skald_relay_client::{ClientState as RelayClientState, RelayClient, RelayClientConfig, SeedSource};

pub use agent::{ClientInfo, ClientState, PairingHandle, RelayAgent};
pub use tools::mobile_tools;

use app::RelayApp;
use notifier::{DelayedNotifier, Kind};

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
    /// Debounces Inbox pushes to the phone; present only while running.
    notifier: Mutex<Option<Arc<DelayedNotifier>>>,
}

impl MobileConnectorPlugin {
    pub fn new() -> Self {
        Self {
            running: AtomicBool::new(false),
            inner: Arc::new(Mutex::new(None)),
            cancel: Mutex::new(None),
            handles: Mutex::new(Vec::new()),
            notifier: Mutex::new(None),
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

        let app = Arc::new(RelayApp::new(
            Arc::clone(&client),
            Arc::clone(&ctx.inbox),
            require_device_confirmation,
        ));

        let notifier = DelayedNotifier::new(Arc::clone(&app), notify_delay);

        let cancel = CancellationToken::new();
        let mut handles = Vec::new();

        // Event loop: apply inbound payloads + authorization policy.
        {
            let app2 = Arc::clone(&app);
            let rx = client.events();
            let c = cancel.clone();
            handles.push(tokio::spawn(async move {
                app2.run_event_loop(rx, c).await;
            }));
        }

        // Bus subscriber: route the four Inbox events through the debouncer.
        // `*Requested` arms a delayed push; `*Resolved` cancels it (or refreshes
        // the phone if the push already went out).
        {
            let notifier = Arc::clone(&notifier);
            let c = cancel.clone();
            let mut rx = ctx.chat_hub.events(PLUGIN_ID);
            handles.push(tokio::spawn(async move {
                use core_api::events::ServerEvent::*;
                loop {
                    tokio::select! {
                        _ = c.cancelled() => break,
                        ev = rx.recv() => match ev {
                            Ok(ge) => match ge.event {
                                ApprovalRequested { request_id, .. } => {
                                    notifier.on_requested((Kind::Approval, request_id)).await;
                                }
                                ApprovalResolved { request_id, .. } => {
                                    notifier.on_resolved((Kind::Approval, request_id)).await;
                                }
                                ClarificationRequested { request_id, .. } => {
                                    notifier.on_requested((Kind::Clarification, request_id)).await;
                                }
                                ClarificationResolved { request_id } => {
                                    notifier.on_resolved((Kind::Clarification, request_id)).await;
                                }
                                ElicitationRequested { request_id, .. } => {
                                    notifier.on_requested((Kind::Elicitation, request_id)).await;
                                }
                                ElicitationResolved { request_id } => {
                                    notifier.on_resolved((Kind::Elicitation, request_id)).await;
                                }
                                _ => {}
                            },
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                warn!(plugin = PLUGIN_ID, skipped = n, "event bus lagged");
                            }
                            Err(_) => break,
                        }
                    }
                }
            }));
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

        *self.notifier.lock().await = Some(notifier);
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
        // Cancel any armed (not-yet-fired) push timers.
        if let Some(notifier) = self.notifier.lock().await.take() {
            notifier.cancel_all().await;
        }
        // Shut down the transport (cancels + joins the WS loop) before dropping
        // the app.
        if let Some(app) = self.inner.lock().await.take() {
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

    async fn broadcast_inbox(&self) -> Result<()> {
        let app = self.app().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        app.broadcast_inbox().await
    }

    async fn broadcast_notification(&self, title: &str, body: &str) -> Result<()> {
        let app = self.app().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        app.broadcast_notification(title, body).await
    }

    async fn list_clients(&self) -> Vec<ClientInfo> {
        let Some(app) = self.app().await else { return Vec::new() };
        app.client()
            .list_clients()
            .await
            .into_iter()
            .map(|r| ClientInfo {
                ed25519_pub: r.ed25519_pub,
                x25519_pub: r.x25519_pub,
                state: match r.state {
                    RelayClientState::Authorized => ClientState::Authorized,
                    RelayClientState::Pending => ClientState::Pending,
                },
                device_info: r.device_info,
                platform: r.platform,
                last_seen: r.last_seen,
            })
            .collect()
    }

    async fn authorize_client(&self, ed25519_pub: [u8; 32]) -> Result<()> {
        let app = self.app().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        app.client().authorize(&ed25519_pub).await?;
        // Send the current Inbox snapshot to the newly-authorized device
        // (payload-agnostic client doesn't do this itself).
        let _ = app.broadcast_inbox().await;
        Ok(())
    }

    async fn revoke_client(&self, ed25519_pub: [u8; 32]) -> Result<()> {
        let app = self.app().await.ok_or_else(|| anyhow::anyhow!("plugin not running"))?;
        app.client().revoke(&ed25519_pub).await
    }
}
