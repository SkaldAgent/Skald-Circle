//! The application layer on top of the payload-agnostic [`RelayClient`].
//!
//! `RelayApp` owns the Skald-specific semantics that the transport crate
//! deliberately knows nothing about: the E2E JSON payload schemas (`payloads`),
//! the `InboxApi` dispatch, and the authorization policy. It consumes
//! `client.events()` and calls `client.send(...)`; the client handles the wire,
//! crypto, counters, and device registry.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use core_api::inbox::InboxApi;
use skald_relay_client::{ClientState, RelayClient, RelayEvent};

use crate::PLUGIN_ID;
use crate::payloads::{self, ClientPayload};

/// Glue between the relay transport ([`RelayClient`]) and Skald's Inbox.
pub struct RelayApp {
    client: Arc<RelayClient>,
    inbox: Arc<dyn InboxApi>,
    /// When true, a freshly paired device stays Pending until a human confirms;
    /// when false, the app auto-authorizes on `ClientPaired`.
    require_device_confirmation: bool,
}

impl RelayApp {
    pub fn new(
        client: Arc<RelayClient>,
        inbox: Arc<dyn InboxApi>,
        require_device_confirmation: bool,
    ) -> Self {
        Self { client, inbox, require_device_confirmation }
    }

    /// The underlying transport client (used by the `RelayAgent` impl + router).
    pub fn client(&self) -> &Arc<RelayClient> {
        &self.client
    }

    // ── Inbox → clients ───────────────────────────────────────────────────────

    /// Build the Inbox snapshot and send it (encrypted) to every Authorized
    /// client. `live=false` so the relay stores-and-forwards + pushes to offline
    /// phones.
    pub async fn broadcast_inbox(&self) -> Result<()> {
        let snapshot = self.inbox.list_pending().await;
        let plaintext = serde_json::to_vec(&payloads::build_inbox_update(&snapshot))?;
        self.broadcast_plaintext(&plaintext).await;
        Ok(())
    }

    /// Build and send a generic notification to all Authorized clients.
    pub async fn broadcast_notification(&self, title: &str, body: &str) -> Result<()> {
        let plaintext = serde_json::to_vec(&payloads::build_notification(title, body))?;
        self.broadcast_plaintext(&plaintext).await;
        Ok(())
    }

    /// Send an opaque plaintext to every Authorized device (`live=false`).
    async fn broadcast_plaintext(&self, plaintext: &[u8]) {
        for c in self
            .client
            .list_clients()
            .await
            .into_iter()
            .filter(|c| c.state == ClientState::Authorized)
        {
            if let Err(e) = self.client.send(&c.ed25519_pub, plaintext, false).await {
                warn!(plugin = PLUGIN_ID, error = %e, "failed to send to client");
            }
        }
    }

    /// Send the current Inbox snapshot to a single client (the targeted reply to
    /// `inbox_request`). `live=true`: the requester is online by construction.
    async fn send_inbox_to(&self, client_ed25519_pub: &[u8; 32]) -> Result<()> {
        let snapshot = self.inbox.list_pending().await;
        let plaintext = serde_json::to_vec(&payloads::build_inbox_update(&snapshot))?;
        self.client.send(client_ed25519_pub, &plaintext, true).await
    }

    // ── Clients → Inbox ───────────────────────────────────────────────────────

    /// Apply a decoded client payload to the Inbox. `payload` is the clean inner
    /// JSON the client already decrypted + de-framed.
    async fn apply_client_payload(&self, from: &[u8; 32], payload: &[u8]) {
        match payloads::parse_client_payload(payload) {
            ClientPayload::ApprovalResponse { request_id, approved, reason } => {
                if approved {
                    self.inbox.approve(request_id).await;
                } else {
                    self.inbox.reject(request_id, reason.unwrap_or_default()).await;
                }
                let _ = self.broadcast_inbox().await;
            }
            ClientPayload::ClarificationResponse { request_id, answer } => {
                self.inbox.answer(request_id, answer).await;
                let _ = self.broadcast_inbox().await;
            }
            ClientPayload::ElicitationResponse { request_id, action, content } => {
                // `content` may hold a secret (SSH/sudo password): hand it straight
                // to the Inbox; never log/persist it in clear (payloads.md §3.1).
                self.inbox.resolve_elicitation(request_id, action, content).await;
                let _ = self.broadcast_inbox().await;
            }
            ClientPayload::Hello { device_info } => {
                if let Err(e) = self.client.set_device_info(from, &device_info.to_string()).await {
                    warn!(plugin = PLUGIN_ID, error = %e, "failed to persist device_info");
                }
            }
            ClientPayload::InboxRequest => {
                if let Err(e) = self.send_inbox_to(from).await {
                    warn!(plugin = PLUGIN_ID, error = %e, "failed to send targeted inbox snapshot");
                }
            }
            ClientPayload::Logout => {
                if let Err(e) = self.client.revoke(from).await {
                    warn!(plugin = PLUGIN_ID, error = %e, "logout revoke failed");
                }
            }
            ClientPayload::Unknown => {
                debug!(plugin = PLUGIN_ID, "unknown/ignored client payload");
            }
        }
    }

    // ── Event loop ────────────────────────────────────────────────────────────

    /// Consume the client's [`RelayEvent`] stream until `cancel` fires. This is
    /// where the authorization policy and Inbox application live.
    pub async fn run_event_loop(
        self: Arc<Self>,
        mut rx: broadcast::Receiver<RelayEvent>,
        cancel: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                ev = rx.recv() => match ev {
                    Ok(RelayEvent::Message { from, payload, .. }) => {
                        self.apply_client_payload(&from, &payload).await;
                    }
                    Ok(RelayEvent::ClientPaired { ed25519_pub, .. }) => {
                        if self.require_device_confirmation {
                            debug!(
                                plugin = PLUGIN_ID,
                                device = %hex::encode(ed25519_pub),
                                "new device paired (pending manual confirmation)"
                            );
                            let _ = self
                                .broadcast_notification(
                                    "Nuovo device",
                                    "Un nuovo device è in attesa di conferma",
                                )
                                .await;
                        } else if let Err(e) = self.client.authorize(&ed25519_pub).await {
                            warn!(plugin = PLUGIN_ID, error = %e, "auto-authorize failed");
                        } else {
                            // Send the newly-authorized device the current snapshot.
                            let _ = self.broadcast_inbox().await;
                        }
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
