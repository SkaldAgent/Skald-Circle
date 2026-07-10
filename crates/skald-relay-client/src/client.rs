//! [`RelayClient`] — the public façade over the networking layer.
//!
//! Concrete struct with inherent async methods (no trait): there is exactly one
//! implementation and the consumer wants a thin, direct handle. The client owns
//! the WS loop lifecycle and the broadcast event channel; all transport/crypto
//! logic lives in [`crate::state::RelayState`], shared behind an `Arc`.

use std::sync::Arc;

use anyhow::Result;
use sqlx::SqlitePool;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::config::RelayClientConfig;
use crate::db::{self, ClientRow};
use crate::events::RelayEvent;
use crate::identity::Identity;
use crate::pairing::{QrCodeData, SessionState, StartedPairing};
use crate::state::{RelayState, StateConfig};
use crate::ws;

/// How many events the broadcast channel buffers before lagging slow consumers.
const EVENT_CHANNEL_CAP: usize = 256;

/// A standalone, payload-agnostic relay client (agent role).
///
/// Lifecycle: [`new`](Self::new) derives the identity and initializes the DB but
/// does **not** connect; [`start`](Self::start) spawns the reconnecting WS loop;
/// [`shutdown`](Self::shutdown) cancels it and joins. Inbound traffic and
/// lifecycle transitions are delivered via [`events`](Self::events).
pub struct RelayClient {
    state: Arc<RelayState>,
    /// Token cancelling the WS loop; `Some` only while started.
    cancel: Mutex<Option<CancellationToken>>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl RelayClient {
    /// Derive the identity from the seed source, ensure the `relay_clients`
    /// table exists, and build the client. Does NOT connect — call
    /// [`start`](Self::start).
    pub async fn new(db: Arc<SqlitePool>, config: RelayClientConfig) -> Result<Self> {
        db::init(&db).await?;
        let identity = Identity::from_source(&config.seed)?;
        info!(
            crate_name = "skald-relay-client",
            namespace = identity.namespace_id_hex(),
            "relay client identity loaded"
        );
        let (events_tx, _) = broadcast::channel(EVENT_CHANNEL_CAP);
        let state = Arc::new(RelayState::new(
            identity,
            db,
            StateConfig { relay_url: config.relay_url, pairing_ttl: config.pairing_ttl },
            events_tx,
        ));
        Ok(Self {
            state,
            cancel: Mutex::new(None),
            handle: Mutex::new(None),
        })
    }

    /// Spawn the reconnecting WS loop. No-op (stays idle) if `relay_url` is
    /// empty. Wires a fresh outbound channel into the state. Calling `start`
    /// while already started replaces the loop (the caller should `shutdown`
    /// first; this guards by cancelling any prior token).
    pub async fn start(&self) -> Result<()> {
        // Cancel any previous loop defensively.
        if let Some(c) = self.cancel.lock().await.take() {
            c.cancel();
        }
        if let Some(h) = self.handle.lock().await.take() {
            let _ = h.await;
        }

        let cancel = CancellationToken::new();
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.state.set_outbound(out_tx);

        if self.state.relay_url().is_empty() {
            // Idle: no WS loop, but the outbound sender is set so pairing/send
            // calls fail loudly ("WS not started") rather than panic.
            *self.cancel.lock().await = Some(cancel);
            return Ok(());
        }

        let st = Arc::clone(&self.state);
        let c = cancel.clone();
        let handle = tokio::spawn(async move {
            ws::run_loop(st, out_rx, c).await;
        });
        *self.cancel.lock().await = Some(cancel);
        *self.handle.lock().await = Some(handle);
        Ok(())
    }

    /// Cancel the WS loop, clear the outbound sender, and join the task.
    pub async fn shutdown(&self) {
        if let Some(c) = self.cancel.lock().await.take() {
            c.cancel();
        }
        self.state.clear_outbound();
        self.state.set_connected(false);
        if let Some(h) = self.handle.lock().await.take() {
            let _ = h.await;
        }
    }

    /// Subscribe to the client's [`RelayEvent`] stream. Each call returns a new
    /// receiver; a slow consumer lags (`RecvError::Lagged`) rather than blocking
    /// the WS loop.
    pub fn events(&self) -> broadcast::Receiver<RelayEvent> {
        self.state.subscribe()
    }

    /// Seal `payload` to one authorized client and queue the `message` frame.
    /// `live=true` routes-or-fails (peer online by construction); `live=false`
    /// stores-and-forwards + pushes for offline phones.
    pub async fn send(&self, dest: &[u8; 32], payload: &[u8], live: bool) -> Result<()> {
        self.state.send_to_client(dest, payload, live).await
    }

    // ── Pipe (relayed byte-stream, docs/relay/pipe.md) ─────────────────────────

    /// Open an end-to-end-encrypted byte pipe to `peer` (a namespace member).
    /// Brokers the rendezvous over the E2E channel (`pipe_invite`/`pipe_accept`,
    /// ephemeral DH → PFS) and returns the live data-plane channel.
    pub async fn open_pipe(
        &self,
        peer: &[u8; 32],
        stream_type: &str,
        headers: std::collections::BTreeMap<String, String>,
    ) -> Result<crate::pipe::PipeConnection> {
        self.state.open_pipe(peer, stream_type, headers).await
    }

    /// Subscribe to inbound pipe invites (responder side). Each invite is an
    /// [`IncomingPipe`](crate::pipe::IncomingPipe); call [`accept_pipe`](Self::accept_pipe)
    /// or [`reject_pipe`](Self::reject_pipe) on it. Single-consumer expected.
    pub fn incoming_pipes(&self) -> broadcast::Receiver<crate::pipe::IncomingPipe> {
        self.state.incoming_pipes()
    }

    /// Accept an inbound invite → returns the live data-plane channel.
    pub async fn accept_pipe(
        &self,
        incoming: &crate::pipe::IncomingPipe,
    ) -> Result<crate::pipe::PipeConnection> {
        self.state.accept_pipe(incoming).await
    }

    /// Decline an inbound invite.
    pub async fn reject_pipe(
        &self,
        incoming: &crate::pipe::IncomingPipe,
        reason: &str,
    ) -> Result<()> {
        self.state.reject_pipe(incoming, reason).await
    }

    // ── Pairing ───────────────────────────────────────────────────────────────

    /// Open the pairing window (single-window, latest-wins). `ttl_secs == 0`
    /// uses the configured default.
    pub async fn start_pairing(&self, ttl_secs: u32) -> Result<StartedPairing> {
        let ttl = if ttl_secs == 0 { self.state.default_pairing_ttl() } else { ttl_secs };
        self.state.start_pairing(ttl).await
    }

    /// Close the pairing window locally and tell the relay.
    pub async fn stop_pairing(&self) -> Result<()> {
        self.state.stop_pairing().await
    }

    /// Resolve a pairing `code` to its QR payload + lifecycle state (QR router).
    pub fn lookup_pairing(&self, code: &str) -> Option<(QrCodeData, SessionState)> {
        self.state.lookup_pairing(code)
    }

    /// The configured default pairing TTL (seconds).
    pub fn default_pairing_ttl(&self) -> u32 {
        self.state.default_pairing_ttl()
    }

    // ── Device registry / authorization ───────────────────────────────────────

    /// Mark a Pending device Authorized and push the updated authorize set.
    /// Payload-agnostic: it does not broadcast any application snapshot — the
    /// consumer does that after authorizing if needed.
    pub async fn authorize(&self, ed25519_pub: &[u8; 32]) -> Result<()> {
        self.state.authorize(ed25519_pub).await
    }

    /// Revoke a device (delete keys/counters, re-push the authorize set without
    /// it). Emits [`RelayEvent::ClientRevoked`].
    pub async fn revoke(&self, ed25519_pub: &[u8; 32]) -> Result<()> {
        self.state.revoke(ed25519_pub).await
    }

    /// Remove every device and push an empty authorize set. Emits one
    /// `ClientRevoked` per removed device.
    pub async fn clear_all(&self) -> Result<()> {
        self.state.clear_all().await
    }

    /// All known devices (pending + authorized), ordered by `authorized_at`.
    pub async fn list_clients(&self) -> Vec<ClientRow> {
        self.state.list_clients().await
    }

    /// Persist the `device_info` JSON for a device (the consumer decodes the
    /// `hello` payload and hands the raw JSON here).
    pub async fn set_device_info(&self, ed25519_pub: &[u8; 32], json: &str) -> Result<()> {
        self.state.set_device_info(ed25519_pub, json).await
    }

    // ── Identity accessors ────────────────────────────────────────────────────

    pub fn agent_ed25519_pub(&self) -> [u8; 32] {
        self.state.identity().ed25519_pub()
    }

    pub fn agent_x25519_pub(&self) -> [u8; 32] {
        self.state.identity().x25519_pub()
    }

    pub fn namespace_id_hex(&self) -> String {
        self.state.identity().namespace_id_hex().to_string()
    }

    pub fn is_connected(&self) -> bool {
        self.state.is_connected()
    }
}
