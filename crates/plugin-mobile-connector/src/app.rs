//! The application layer on top of the payload-agnostic [`RelayClient`].
//!
//! `RelayApp` is the plugin's central shared hub (the mobile analogue of the
//! Telegram `TgShared`): it owns the E2E payload semantics, the device→user
//! bindings cache, and the per-user forwarder/notifier registries. It knows
//! nothing about the wire — the client handles transport, crypto, counters, and
//! the device registry.
//!
//! # Multi-user (blueprint §13)
//!
//! Every device is bound to one Skald user (`auth::Binding`). Inbound payloads
//! resolve the sending device's user, then apply to **that user's** Inbox via the
//! [`UserChannelApi`] seam. Outbound Inbox pushes go only to the authorized
//! devices bound to the target user — never a global broadcast. A per-user
//! forwarder (`events`) drives pushes from the user's event stream.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{broadcast, Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use core_api::config_api::ConfigApi;
use core_api::user_channel::UserChannelApi;
use skald_relay_client::{ClientState, RelayClient, RelayEvent};

use crate::PLUGIN_ID;
use crate::auth::{self, MobileConfig};
use crate::notifier::DelayedNotifier;
use crate::payloads::{self, ClientPayload};

/// The plugin's shared application state.
pub struct RelayApp {
    client: Arc<RelayClient>,
    /// Per-user runtime resolver (blueprint §13). `None` = user locked (§9).
    pub(crate) user_channel: Arc<dyn UserChannelApi>,
    /// Config store — used to persist binding removals (logout/revoke).
    config: Arc<dyn ConfigApi>,
    /// Device→user bindings, cached in memory; kept in sync by `auth::config_listener`.
    pub(crate) bindings: RwLock<MobileConfig>,
    /// When true, a freshly paired device stays Pending until an admin binds it
    /// (`mobile_bind_device`, which authorizes it). Binding *is* the confirmation.
    require_device_confirmation: bool,
    /// Debounce before an unresolved Inbox item is pushed to the phone.
    pub(crate) notify_delay: Duration,
    /// Cancellation for every task spawned by this run (forwarders, listeners).
    cancel: CancellationToken,
    /// user_ids with an active per-user forwarder task.
    pub(crate) forwarders: Mutex<HashSet<String>>,
    /// Per-user debounced notifiers, created on demand by the forwarders.
    pub(crate) notifiers: Mutex<HashMap<String, Arc<DelayedNotifier>>>,
}

impl RelayApp {
    pub fn new(
        client: Arc<RelayClient>,
        user_channel: Arc<dyn UserChannelApi>,
        config: Arc<dyn ConfigApi>,
        bindings: MobileConfig,
        require_device_confirmation: bool,
        notify_delay: Duration,
        cancel: CancellationToken,
    ) -> Arc<Self> {
        Arc::new(Self {
            client,
            user_channel,
            config,
            bindings: RwLock::new(bindings),
            require_device_confirmation,
            notify_delay,
            cancel,
            forwarders: Mutex::new(HashSet::new()),
            notifiers: Mutex::new(HashMap::new()),
        })
    }

    /// The underlying transport client (used by the `RelayAgent` impl + router).
    pub fn client(&self) -> &Arc<RelayClient> {
        &self.client
    }

    /// Cancellation token for this run's spawned tasks.
    pub(crate) fn cancel(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Write guard on the bindings cache (used by the config listener).
    pub(crate) async fn bindings_mut(&self) -> tokio::sync::RwLockWriteGuard<'_, MobileConfig> {
        self.bindings.write().await
    }

    /// The user bound to a device pubkey, if any.
    pub(crate) async fn user_for_device(&self, pubkey: &[u8; 32]) -> Option<String> {
        self.bindings.read().await.user_for_pubkey(&hex::encode(pubkey))
    }

    // ── Bind / unbind (admin-mediated, via the control tools) ─────────────────

    /// Bind a paired device to a user and authorize it, then push that user's
    /// current Inbox to it. Persists the binding (fires `ConfigKeyUpdated`, which
    /// refreshes the cache and spawns the user's forwarder).
    pub async fn bind_device(
        &self,
        pubkey: [u8; 32],
        user_id: String,
        display: Option<String>,
    ) -> Result<()> {
        // Update the persisted config + local cache up front (avoids a race with
        // the listener before its event arrives).
        let snapshot = {
            let mut cfg = self.bindings.write().await;
            cfg.upsert(auth::Binding { pubkey_hex: hex::encode(pubkey), user_id: user_id.clone(), display });
            cfg.clone()
        };
        auth::save_config(&*self.config, &snapshot).await?;

        // Authorize at the relay level so pushes/sends reach the device.
        self.client.authorize(&pubkey).await?;
        info!(plugin = PLUGIN_ID, user_id = %user_id, device = %hex::encode(pubkey), "device bound + authorized");

        // Send the user's current Inbox to the freshly bound device.
        if let Err(e) = self.push_inbox_to_user(&user_id).await {
            warn!(plugin = PLUGIN_ID, error = %e, "failed to push inbox after bind");
        }
        Ok(())
    }

    /// Revoke a device and drop its binding.
    pub async fn revoke_device(&self, pubkey: [u8; 32]) -> Result<()> {
        self.client.revoke(&pubkey).await?;
        let snapshot = {
            let mut cfg = self.bindings.write().await;
            cfg.remove(&hex::encode(pubkey));
            cfg.clone()
        };
        auth::save_config(&*self.config, &snapshot).await?;
        Ok(())
    }

    // ── Inbox → user's devices ────────────────────────────────────────────────

    /// Build the Inbox snapshot for `user_id` and send it to every Authorized
    /// device bound to that user. `live=false` so the relay stores-and-forwards +
    /// pushes to offline phones. No-op if the user is locked (§9).
    pub async fn push_inbox_to_user(&self, user_id: &str) -> Result<()> {
        let Some(handle) = self.user_channel.resolve_user(user_id).await else {
            debug!(plugin = PLUGIN_ID, user_id, "inbox push skipped — user locked");
            return Ok(());
        };
        let snapshot = handle.inbox().list_pending().await;
        let plaintext = serde_json::to_vec(&payloads::build_inbox_update(&snapshot))?;
        self.send_to_user_devices(user_id, &plaintext, false).await;
        Ok(())
    }

    /// Send an opaque plaintext to every Authorized device bound to `user_id`.
    async fn send_to_user_devices(&self, user_id: &str, plaintext: &[u8], live: bool) {
        let bound = self.bindings.read().await.pubkeys_for_user(user_id);
        if bound.is_empty() {
            return;
        }
        let authorized: HashSet<[u8; 32]> = self
            .client
            .list_clients()
            .await
            .into_iter()
            .filter(|c| c.state == ClientState::Authorized)
            .map(|c| c.ed25519_pub)
            .collect();
        for pk_hex in bound {
            let Some(pk) = skald_relay_common::crypto::decode_hex::<32>(&pk_hex) else { continue };
            if !authorized.contains(&pk) {
                continue;
            }
            if let Err(e) = self.client.send(&pk, plaintext, live).await {
                warn!(plugin = PLUGIN_ID, error = %e, "failed to send to device");
            }
        }
    }

    /// Send the current Inbox snapshot to a single requesting device (`live=true`:
    /// the requester is online by construction).
    async fn send_inbox_to_device(&self, user_id: &str, device: &[u8; 32]) -> Result<()> {
        let Some(handle) = self.user_channel.resolve_user(user_id).await else {
            return Ok(());
        };
        let snapshot = handle.inbox().list_pending().await;
        let plaintext = serde_json::to_vec(&payloads::build_inbox_update(&snapshot))?;
        self.client.send(device, &plaintext, true).await
    }

    // ── Devices → Inbox ───────────────────────────────────────────────────────

    /// Apply a decoded client payload to the sending device's *user's* Inbox.
    /// Unbound device or locked user → the request is ignored (no cross-user leak).
    async fn apply_client_payload(&self, from: &[u8; 32], payload: &[u8]) {
        let parsed = payloads::parse_client_payload(payload);

        // Hello / Logout are device-registry ops that need no user resolution.
        match &parsed {
            ClientPayload::Hello { device_info } => {
                if let Err(e) = self.client.set_device_info(from, &device_info.to_string()).await {
                    warn!(plugin = PLUGIN_ID, error = %e, "failed to persist device_info");
                }
                return;
            }
            ClientPayload::Logout => {
                if let Err(e) = self.revoke_device(*from).await {
                    warn!(plugin = PLUGIN_ID, error = %e, "logout revoke failed");
                }
                return;
            }
            ClientPayload::Unknown => {
                debug!(plugin = PLUGIN_ID, "unknown/ignored client payload");
                return;
            }
            _ => {}
        }

        // Everything else acts on a user's Inbox: resolve the device's user.
        let Some(user_id) = self.user_for_device(from).await else {
            warn!(plugin = PLUGIN_ID, device = %hex::encode(from), "payload from unbound device — ignored");
            return;
        };
        let Some(handle) = self.user_channel.resolve_user(&user_id).await else {
            debug!(plugin = PLUGIN_ID, user_id = %user_id, "payload dropped — user locked");
            return;
        };
        let inbox = handle.inbox();

        match parsed {
            ClientPayload::ApprovalResponse { request_id, approved, reason } => {
                if approved {
                    inbox.approve(request_id).await;
                } else {
                    inbox.reject(request_id, reason.unwrap_or_default()).await;
                }
                let _ = self.push_inbox_to_user(&user_id).await;
            }
            ClientPayload::ClarificationResponse { request_id, answer } => {
                inbox.answer(request_id, answer).await;
                let _ = self.push_inbox_to_user(&user_id).await;
            }
            ClientPayload::ElicitationResponse { request_id, action, content } => {
                // `content` may hold a secret (SSH/sudo password): hand it straight
                // to the Inbox; never log/persist it in clear (payloads.md §3.1).
                inbox.resolve_elicitation(request_id, action, content).await;
                let _ = self.push_inbox_to_user(&user_id).await;
            }
            ClientPayload::InboxRequest => {
                if let Err(e) = self.send_inbox_to_device(&user_id, from).await {
                    warn!(plugin = PLUGIN_ID, error = %e, "failed to send targeted inbox snapshot");
                }
            }
            // Handled above.
            ClientPayload::Hello { .. } | ClientPayload::Logout | ClientPayload::Unknown => {}
        }
    }

    // ── Event loop ────────────────────────────────────────────────────────────

    /// Consume the client's [`RelayEvent`] stream until cancelled. Applies inbound
    /// payloads and the pairing authorization policy, and lazily spawns a per-user
    /// forwarder when a bound device becomes active.
    pub async fn run_event_loop(self: Arc<Self>, mut rx: broadcast::Receiver<RelayEvent>) {
        let cancel = self.cancel.clone();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                ev = rx.recv() => match ev {
                    Ok(RelayEvent::Message { from, payload, .. }) => {
                        // A bound + unlocked device becoming active ⇒ ensure its
                        // user's forwarder is running so Inbox events reach the phone.
                        if let Some(user_id) = self.user_for_device(&from).await {
                            if let Some(handle) = self.user_channel.resolve_user(&user_id).await {
                                crate::events::ensure_forwarder(Arc::clone(&self), user_id, handle).await;
                            }
                        }
                        self.apply_client_payload(&from, &payload).await;
                    }
                    Ok(RelayEvent::ClientPaired { ed25519_pub, .. }) => {
                        // The device is not bound to any user yet, so there is no
                        // one to push to. An admin binds it with `mobile_bind_device`
                        // (which authorizes it). We only optionally pre-authorize.
                        if !self.require_device_confirmation {
                            if let Err(e) = self.client.authorize(&ed25519_pub).await {
                                warn!(plugin = PLUGIN_ID, error = %e, "auto-authorize failed");
                            }
                        }
                        info!(
                            plugin = PLUGIN_ID,
                            device = %hex::encode(ed25519_pub),
                            "new device paired — awaiting admin binding (mobile_bind_device)"
                        );
                    }
                    Ok(RelayEvent::ClientRevoked { .. })
                    | Ok(RelayEvent::Connected)
                    | Ok(RelayEvent::Disconnected) => {}
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(plugin = PLUGIN_ID, skipped = n, "relay event stream lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}
