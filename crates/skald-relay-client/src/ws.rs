//! The permanent agent WebSocket toward the relay, speaking **v2 protobuf**
//! (docs/relay/relay-protocol.md).
//!
//! A single WS carries everything: challenge-response auth, the `Authorize` set,
//! outbound E2E `Message`s, and inbound `Message` / `ClientPaired` frames. v2
//! transport is **binary-only**: every wire frame is a `RelayFrame` protobuf
//! message wrapped in `Message::Binary`; WS-level `Ping`/`Pong`/`Close` are
//! their own `WsMessage` variants and never appear as protobuf.
//!
//! Reconnection uses exponential backoff (1,2,4,…,60 s) with jitter, and the
//! whole loop is cancellable on stop. A live session is kept honest by the
//! [`Liveness`] probe — without it a silently broken path parks the loop forever
//! (see that type's docs).

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use rand::Rng;
use skald_relay_common::crypto;
use skald_relay_common::proto::v2::*;
use skald_relay_common::proto::v2::relay_frame::Frame;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::state::RelayState;

/// How often the agent sends its **own** WS `Ping` on a live session.
const PING_INTERVAL_SECS: u64 = 20;

/// No inbound frame for this long ⇒ the session is dead; drop it and redial.
/// Two and a half of the relay's 30 s pings, comfortably under its own 120 s
/// idle close.
const IDLE_TIMEOUT_SECS: u64 = 75;

/// Per-session liveness knobs.
///
/// The probe exists because a purely *reactive* session cannot notice its own
/// death. We answer the relay's `Ping` with a `Pong` and otherwise send nothing
/// for long stretches, so when the path breaks silently — NAT rebinding, a
/// reverse proxy dropping its state — there are no unacked bytes on the socket
/// for the kernel to retransmit, no TCP error, and the relay's `Close` (it gives
/// up after 120 s of quiet) falls into the same hole. `stream.next()` then parks
/// forever on a socket to nobody, `is_connected()` keeps answering `true`, and
/// the reconnect schedule below — which works fine, it just never gets asked —
/// is never reached. Only a process restart clears it.
///
/// So both halves matter: `ping_every` keeps unacked bytes on the wire (the
/// relay pongs them back, which also refreshes *its* idle timer), and
/// `idle_after` turns silence into an `Err` and hands the session to the
/// reconnect path.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Liveness {
    /// Interval between our outbound `Ping`s.
    ping_every: Duration,
    /// Silence tolerated before the session is declared dead.
    idle_after: Duration,
}

// Hand-written: a derived `Default` would give a zero `ping_every` (a hot loop)
// and a zero `idle_after` (every session dead on arrival).
impl Default for Liveness {
    fn default() -> Self {
        Self {
            ping_every: Duration::from_secs(PING_INTERVAL_SECS),
            idle_after: Duration::from_secs(IDLE_TIMEOUT_SECS),
        }
    }
}

/// Run the reconnecting WS loop until `cancel` fires (relay-protocol.md §8).
pub(crate) async fn run_loop(
    state: Arc<RelayState>,
    outbound_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: CancellationToken,
) {
    run_loop_with(state, outbound_rx, cancel, Liveness::default()).await
}

/// [`run_loop`] with the liveness knobs spelled out (tests use short ones so a
/// redial is observable in milliseconds).
async fn run_loop_with(
    state: Arc<RelayState>,
    mut outbound_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: CancellationToken,
    liveness: Liveness,
) {
    let mut backoff_step: u32 = 0;
    loop {
        if cancel.is_cancelled() {
            return;
        }

        match connect_once(&state, &mut outbound_rx, &cancel, liveness).await {
            Ok(()) => {
                // Clean disconnect (cancelled or graceful): reset backoff.
                backoff_step = 0;
            }
            Err(e) => {
                warn!(crate_name = "skald-relay-client", error = %e, "relay connection ended");
                state.set_last_error(e.to_string());
            }
        }

        if cancel.is_cancelled() {
            return;
        }

        let delay = backoff_delay(backoff_step);
        backoff_step = backoff_step.saturating_add(1);
        state.set_connected(false);
        debug!(crate_name = "skald-relay-client", secs = delay.as_secs_f64(), "reconnect backoff");
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// Backoff schedule 1,2,4,…,60 s plus up to 50% jitter (relay-protocol.md §8).
fn backoff_delay(step: u32) -> Duration {
    let base = 1u64.checked_shl(step).unwrap_or(60).min(60);
    let jitter_ms = rand::rng().random_range(0..=(base * 500));
    Duration::from_millis(base * 1000 + jitter_ms)
}

/// One full connection lifecycle: connect → challenge → auth → authorize → loop.
async fn connect_once(
    state: &Arc<RelayState>,
    outbound_rx: &mut mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: &CancellationToken,
    liveness: Liveness,
) -> Result<()> {
    let url = state.relay_url();
    info!(crate_name = "skald-relay-client", %url, "connecting to relay");

    let (ws_stream, _resp) = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        r = tokio_tungstenite::connect_async(&url) => r?,
    };
    let (mut sink, mut stream) = ws_stream.split();

    // 1. Wait for the relay's challenge (it speaks first, relay-protocol.md §4).
    let challenge_nonce = wait_for_challenge(&mut stream).await?;

    // 2. Sign AUTH_DOMAIN ‖ 0x00 ‖ nonce and send the agent Auth frame.
    let sig = crypto::sign_challenge(&state.identity().signing_key(), &challenge_nonce);
    let auth = RelayFrame {
        frame: Some(Frame::Auth(Auth {
            role: Some(auth::Role::Agent(AuthAgent {
                agent_ed25519_pub: prost::bytes::Bytes::copy_from_slice(
                    &state.identity().ed25519_pub(),
                ),
            })),
            signature: prost::bytes::Bytes::copy_from_slice(&sig),
        })),
    };
    sink.send(WsMessage::Binary(auth.encode_to_vec().into())).await?;

    // 3. Expect AuthOk and verify the namespace_id locally.
    let ns_raw = wait_for_auth_ok(&mut stream).await?;
    if ns_raw != state.identity().namespace_id_raw() {
        return Err(anyhow!(
            "relay returned mismatched namespace_id (got {}, expected {})",
            hex::encode(ns_raw),
            hex::encode(state.identity().namespace_id_raw())
        ));
    }
    info!(crate_name = "skald-relay-client", "relay auth ok, namespace verified");
    state.set_connected(true);

    // 4. Send the current authorize set from the DB (empty on first run).
    // We push it directly via the sink rather than through `outbound_rx` so it
    // lands immediately — the queue is only drained inside the main loop below.
    let authorized = state.authorized_pubkeys_hex().await.unwrap_or_default();
    let clients: Vec<prost::bytes::Bytes> = authorized
        .iter()
        .filter_map(|h| hex::decode(h).ok())
        .map(prost::bytes::Bytes::from)
        .collect();
    let authorize = RelayFrame {
        frame: Some(Frame::Authorize(Authorize { clients })),
    };
    sink.send(WsMessage::Binary(authorize.encode_to_vec().into())).await?;

    // 5. Main dispatch loop: outbound queue, inbound frames, WS-level Ping/Pong,
    // and the liveness probe.
    let mut ping = tokio::time::interval(liveness.ping_every);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // consume the immediate first tick — we just handshook
    let mut last_seen = Instant::now();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = sink.send(WsMessage::Close(None)).await;
                return Ok(());
            }

            // Liveness (see `Liveness`): probe the socket, and give up on a
            // session that has gone quiet. Returning `Err` is what puts us back
            // on the reconnect schedule instead of parking here forever.
            _ = ping.tick() => {
                let quiet = last_seen.elapsed();
                if quiet > liveness.idle_after {
                    return Err(anyhow!(
                        "relay silent for {}s (no frame, not even a pong); redialing",
                        quiet.as_secs()
                    ));
                }
                sink.send(WsMessage::Ping(Vec::new().into())).await?;
            }

            // Outbound: already-encoded protobuf frames queued by pairing / send
            // / revoke. The channel carries `Vec<u8>` ready to be shipped as a
            // binary WS frame.
            maybe = outbound_rx.recv() => {
                match maybe {
                    Some(bytes) => sink.send(WsMessage::Binary(bytes.into())).await?,
                    None => return Ok(()), // channel closed → client stopping
                }
            }

            // Inbound: relay → agent frames.
            maybe = stream.next() => {
                let Some(msg) = maybe else { return Ok(()) }; // stream ended
                // Any frame at all — data, Ping, Pong — proves the path is
                // still there, which is the whole question the probe asks.
                last_seen = Instant::now();
                match msg? {
                    WsMessage::Binary(data) => {
                        handle_incoming(state, &data).await;
                    }
                    WsMessage::Ping(p) => sink.send(WsMessage::Pong(p)).await?,
                    WsMessage::Pong(_) => {}
                    WsMessage::Close(_) => return Ok(()),
                    WsMessage::Text(_) | WsMessage::Frame(_) => {
                        // v2 transport is binary-only; ignore text/frame
                        // variants (forward-compat, no protocol-defined reaction).
                    }
                }
            }
        }
    }
}

/// Read binary frames until `Challenge` arrives; returns the raw 32-byte nonce.
async fn wait_for_challenge<S>(stream: &mut S) -> Result<[u8; 32]>
where
    S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = stream.next().await {
        match msg? {
            WsMessage::Binary(data) => {
                let frame = RelayFrame::decode(&data[..])?;
                if let Some(Frame::Challenge(c)) = frame.frame {
                    if c.nonce.len() != 32 {
                        return Err(anyhow!("challenge nonce is not 32 bytes"));
                    }
                    let mut out = [0u8; 32];
                    out.copy_from_slice(&c.nonce);
                    return Ok(out);
                }
            }
            WsMessage::Close(_) => return Err(anyhow!("closed before challenge")),
            _ => {}
        }
    }
    Err(anyhow!("connection closed before challenge"))
}

/// Read binary frames until `AuthOk`; returns the raw 32-byte namespace_id.
async fn wait_for_auth_ok<S>(stream: &mut S) -> Result<[u8; 32]>
where
    S: StreamExt<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    while let Some(msg) = stream.next().await {
        match msg? {
            WsMessage::Binary(data) => {
                let frame = RelayFrame::decode(&data[..])?;
                match frame.frame {
                    Some(Frame::AuthOk(AuthOk { namespace_id })) => {
                        if namespace_id.len() != 32 {
                            return Err(anyhow!("namespace_id is not 32 bytes"));
                        }
                        let mut out = [0u8; 32];
                        out.copy_from_slice(&namespace_id);
                        return Ok(out);
                    }
                    Some(Frame::AuthError(AuthError { code, message })) => {
                        return Err(anyhow!("auth_error from relay: {code} ({message})"));
                    }
                    _ => {}
                }
            }
            WsMessage::Close(_) => return Err(anyhow!("closed before auth_ok")),
            _ => {}
        }
    }
    Err(anyhow!("connection closed before auth_ok"))
}

/// Dispatch one decoded relay→agent `RelayFrame`. WS-level Ping/Pong are
/// handled at the transport layer above; everything that arrives as a binary
/// frame is decoded to `RelayFrame` and matched on the `Frame` oneof here.
async fn handle_incoming(state: &Arc<RelayState>, data: &[u8]) {
    let frame = match RelayFrame::decode(data) {
        Ok(f) => f,
        Err(e) => {
            warn!(crate_name = "skald-relay-client", error = %e, "malformed protobuf frame dropped");
            return;
        }
    };
    let Some(f) = frame.frame else {
        debug!(crate_name = "skald-relay-client", "empty relay frame dropped");
        return;
    };
    match f {
        Frame::Message(m) => {
            // Validate lengths before handing off to the E2E layer.
            if m.peer.len() != 32 || m.nonce.len() != 12 {
                warn!(crate_name = "skald-relay-client", "message with wrong peer/nonce length dropped");
                return;
            }
            let mut from = [0u8; 32];
            from.copy_from_slice(&m.peer);
            let mut nonce = [0u8; 12];
            nonce.copy_from_slice(&m.nonce);
            state.handle_inbound_message(&from, &nonce, &m.ciphertext, m.live).await;
        }
        Frame::ClientPaired(cp) => {
            if cp.client_ed25519_pub.len() != 32 || cp.client_x25519_pub.len() != 32 {
                warn!(crate_name = "skald-relay-client", "client_paired with wrong pubkey length dropped");
                return;
            }
            let mut ed = [0u8; 32];
            ed.copy_from_slice(&cp.client_ed25519_pub);
            let mut x = [0u8; 32];
            x.copy_from_slice(&cp.client_x25519_pub);
            // Decode the protobuf `Platform` enum to the lowercase string the DB
            // expects. The wire value defaults to `0` (`UNSPECIFIED`) — the helper
            // maps that to `"unknown"`.
            let platform = platform_i32_to_str(cp.platform);
            state.handle_client_paired(&ed, &x, platform).await;
        }
        Frame::AuthorizeOk(aok) => {
            debug!(crate_name = "skald-relay-client", authorized = aok.authorized, "authorize_ok");
        }
        Frame::PairingReady(_) | Frame::PairingStopOk(_) => {}
        Frame::PresenceEvent(pe) => {
            debug!(
                crate_name = "skald-relay-client",
                pubkey = %hex::encode(&pe.pubkey),
                status = pe.status,
                "presence event"
            );
        }
        Frame::PresenceList(pl) => {
            debug!(crate_name = "skald-relay-client", online = pl.online.len(), "presence list");
        }
        Frame::PeerOffline(po) => {
            // Expected backstop for route-or-fail live sends (relay-protocol.md
            // §3): a `live=true` send found the peer gone. A normal protocol
            // event, not an error.
            debug!(
                crate_name = "skald-relay-client",
                peer = %hex::encode(&po.peer),
                "peer offline for live send; dropping"
            );
        }
        Frame::Error(e) => {
            warn!(crate_name = "skald-relay-client", code = %e.code, message = %e.message, "relay error frame");
        }
        // Server-to-client or handshake frames the agent never expects inbound.
        Frame::Challenge(_)
        | Frame::Auth(_)
        | Frame::AuthOk(_)
        | Frame::AuthError(_)
        | Frame::Authorize(_)
        | Frame::PairingStart(_)
        | Frame::PairingStop(_)
        | Frame::PresenceRequest(_) => {
            warn!(crate_name = "skald-relay-client", "unexpected relay→agent frame dropped");
        }
    }
}

/// Map a protobuf `Platform` enum wire value to the lowercase string the DB
/// stores in the `platform` column. Unknown values become `"unknown"`.
fn platform_i32_to_str(v: i32) -> &'static str {
    if v == Platform::Ios as i32 {
        "ios"
    } else if v == Platform::Android as i32 {
        "android"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `platform_i32_to_str` is total on the wire values the relay emits and
    /// never panics on bogus inputs (relay-protocol.md §11 forward-compat).
    #[test]
    fn platform_conversion() {
        assert_eq!(platform_i32_to_str(0), "unknown");
        assert_eq!(platform_i32_to_str(1), "ios");
        assert_eq!(platform_i32_to_str(2), "android");
        assert_eq!(platform_i32_to_str(99), "unknown");
    }

    /// A minimal `Message` frame round-trips through `prost` so the wire
    /// encoding we emit is the same one the relay will decode.
    #[test]
    fn message_frame_round_trip() {
        let frame = RelayFrame {
            frame: Some(Frame::Message(Message {
                ciphertext: vec![0xAA; 64].into(),
                nonce: vec![0x01; 12].into(),
                peer: vec![0x02; 32].into(),
                live: false,
            })),
        };
        let bytes = frame.encode_to_vec();
        let decoded = RelayFrame::decode(&bytes[..]).expect("decode");
        match decoded.frame {
            Some(Frame::Message(m)) => {
                assert_eq!(m.ciphertext.len(), 64);
                assert_eq!(m.nonce.len(), 12);
                assert_eq!(m.peer.len(), 32);
                assert!(!m.live);
            }
            other => panic!("expected Message, got {other:?}"),
        }
    }
}

/// The liveness probe against a **silent** relay — the shape a black-holed path
/// leaves behind, where no `Close` and no TCP error ever arrive. The fake relay
/// completes the v2 handshake and then never speaks again; the agent has to work
/// out on its own that the session is dead, drop it, and redial. Before the
/// probe existed this parked forever and only a process restart cleared it.
#[cfg(test)]
mod net_tests {
    use super::*;
    use std::net::SocketAddr;

    use skald_relay_common::proto::v2::{AuthOk, Challenge};
    use sqlx::SqlitePool;
    use tokio::io::AsyncReadExt;
    use tokio::net::{TcpListener, TcpStream};

    use crate::db;
    use crate::identity::Identity;
    use crate::state::StateConfig;

    /// Same seed on both sides so the `AuthOk` carries the namespace the agent
    /// expects (a mismatch is a different failure than the one under test).
    const SEED: [u8; 32] = [0x42; 32];

    /// What the harness reports about the agent's dialling behaviour.
    #[derive(Debug)]
    enum Event {
        /// A TCP connection was accepted.
        Accepted,
        /// That connection reached EOF — i.e. the agent hung up.
        HungUp,
    }

    /// A relay that handshakes and then goes mute.
    async fn spawn_silent_relay(ns_raw: [u8; 32]) -> (String, mpsc::UnboundedReceiver<Event>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let tx = tx.clone();
                let _ = tx.send(Event::Accepted);
                tokio::spawn(async move {
                    silent_session(tcp, ns_raw).await;
                    let _ = tx.send(Event::HungUp);
                });
            }
        });
        (format!("ws://{addr}/v1/ws"), rx)
    }

    /// Challenge → read the agent's `Auth` → `AuthOk` → total silence, until the
    /// agent closes the socket.
    ///
    /// The silence is why the tail reads the **raw TCP stream** instead of
    /// `ws.next()`: tungstenite answers an inbound `Ping` with an automatic
    /// `Pong` flushed on the next read, which would keep the agent's `last_seen`
    /// fresh and defeat the very condition being simulated. For the same reason
    /// this asserts nothing about the probe frames themselves — the observable
    /// contract is that the agent gives up and comes back.
    async fn silent_session(tcp: TcpStream, ns_raw: [u8; 32]) {
        let mut ws = tokio_tungstenite::accept_async(tcp).await.expect("ws accept");

        let challenge = RelayFrame {
            frame: Some(Frame::Challenge(Challenge {
                nonce: prost::bytes::Bytes::from(vec![0x5A; 32]),
            })),
        };
        ws.send(WsMessage::Binary(challenge.encode_to_vec().into())).await.unwrap();

        // The agent's `Auth` is the next binary frame. It signs a nonce we chose
        // ourselves, so there is nothing here worth verifying.
        while let Some(Ok(msg)) = ws.next().await {
            if matches!(msg, WsMessage::Binary(_)) {
                break;
            }
        }

        let ok = RelayFrame {
            frame: Some(Frame::AuthOk(AuthOk {
                namespace_id: prost::bytes::Bytes::copy_from_slice(&ns_raw),
            })),
        };
        ws.send(WsMessage::Binary(ok.encode_to_vec().into())).await.unwrap();

        // From here on we are a black hole: drain bytes, answer nothing.
        let tcp = ws.get_mut();
        let mut scratch = [0u8; 1024];
        while let Ok(n) = tcp.read(&mut scratch).await {
            if n == 0 {
                break; // agent hung up
            }
        }
    }

    async fn make_state(relay_url: String) -> Arc<RelayState> {
        let path = std::env::temp_dir()
            .join(format!("relay-cli-liveness-{}.db", std::process::id()));
        let pool = SqlitePool::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        db::init(&pool).await.unwrap();
        let (events_tx, _) = tokio::sync::broadcast::channel(16);
        Arc::new(RelayState::new(
            Identity::from_seed(&SEED),
            Arc::new(pool),
            StateConfig { relay_url, pairing_ttl: 300 },
            events_tx,
        ))
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<Event>) -> Event {
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting on the relay harness")
            .expect("relay harness gone")
    }

    #[tokio::test]
    async fn silent_relay_is_dropped_and_redialed() {
        let ns_raw = Identity::from_seed(&SEED).namespace_id_raw();
        let (url, mut events) = spawn_silent_relay(ns_raw).await;
        let state = make_state(url).await;

        let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        state.set_outbound(out_tx);
        let cancel = CancellationToken::new();
        // Production values scaled down ~200×; the ratio is what matters.
        let liveness = Liveness {
            ping_every: Duration::from_millis(100),
            idle_after: Duration::from_millis(400),
        };
        let task = {
            let state = Arc::clone(&state);
            let cancel = cancel.clone();
            tokio::spawn(async move { run_loop_with(state, out_rx, cancel, liveness).await })
        };

        assert!(matches!(next(&mut events).await, Event::Accepted), "agent should dial");
        assert!(
            matches!(next(&mut events).await, Event::HungUp),
            "agent parked on a mute socket instead of giving up on it",
        );
        assert!(
            matches!(next(&mut events).await, Event::Accepted),
            "agent dropped the dead session but never dialled again",
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
}
