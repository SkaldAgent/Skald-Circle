//! Per-connection WebSocket handler for the v2 transport
//! (data/iOS-app/v2/relay-protocol.md).
//!
//! One Tokio task per socket. The socket is split: a dedicated writer task owns
//! the sink and drains a `mpsc::Sender<WsOut>` (also stored in the routing
//! registry so peers can deliver to us); the reader task drives the state
//! machine `challenge → auth(role) → authed forward loop`, with WS-level
//! keepalive.
//!
//! v2 transport (relay-protocol.md §1): every post-challenge WebSocket frame is
//! **binary** and contains **exactly one** `RelayFrame` protobuf message. WS-
//! level Ping/Pong are used for keepalive (relay-protocol.md §5) and handled at
//! the WS transport layer, not inside the protobuf decoder.

use std::cmp::Ordering;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message as AxumMessage, WebSocket};
use futures_util::SinkExt;
use futures_util::stream::{SplitStream, StreamExt};
use rand::RngCore;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::AppState;
use crate::auth::{namespace_id, verify_challenge};
use crate::limits::{
    self, CHALLENGE_TIMEOUT_SECS, IDLE_TIMEOUT_SECS, MAX_FRAME_BYTES, MAX_LIVE_FRAME_BYTES,
    PAIRING_TTL_MAX, PING_INTERVAL_SECS, QUEUE_MAX_PER_DEST,
};
use crate::push::{Platform, PushItem};
use crate::routing::{ConnHandle, WsOut};
use crate::store::now_ms;
use crate::types::proto;
use crate::types::proto::auth::Role as AuthRole;
use crate::types::proto::relay_frame::Frame;
use crate::types::proto::{
    Auth, AuthError, AuthOk, Authorize, AuthorizeOk, ClientPaired, Message as ProtoMessage,
    PairingReady, PairingStart, PairingStopOk, PeerOffline, PresenceEvent, PresenceList,
    RelayFrame,
};

/// Role of an authenticated, long-lived connection. Pairing is short-lived and
/// handled inline, so it is not part of this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Agent,
    Client,
}

/// What the reader loop should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Continue,
    Close,
}

/// Outcome of the pre-auth read loop (`read_auth`).
enum AuthRead {
    /// A decoded `Auth` frame, ready for role dispatch.
    Frame(Box<Auth>),
    /// `CHALLENGE_TIMEOUT_SECS` elapsed with no `Auth` received.
    Timeout,
    /// Any pre-auth protocol violation (size exceeded, wrong frame kind, text
    /// frame, malformed protobuf, …). The caller sends `auth_error` and closes.
    Bad,
    /// Peer closed the WS (or the stream ended).
    Closed,
}

// ---------------------------------------------------------------------------
// Top-level entrypoint
// ---------------------------------------------------------------------------

/// Drive one accepted WebSocket to completion. Called from `lib.rs` after the
/// axum upgrade (relay-protocol.md §4).
pub async fn handle_socket(socket: WebSocket, state: AppState, peer: IpAddr) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<WsOut>(64);
    let cancel = CancellationToken::new();
    let id = state.next_conn_id();

    // Writer task: the only owner of the sink. v2 — every control/data frame
    // is a protobuf `RelayFrame` encoded and sent as a WebSocket **binary**
    // message; WS-level Ping/Pong/Close are their own variants.
    //
    // `prost::Message::encode_to_vec` is infallible in prost 0.13: it panics
    // on out-of-memory but never returns an error. We log + break the writer
    // only on the very rare allocation failure.
    let writer = tokio::spawn(async move {
        while let Some(item) = out_rx.recv().await {
            let res = match item {
                WsOut::Frame(f) => {
                    let buf = prost::Message::encode_to_vec(&f);
                    sink.send(AxumMessage::Binary(buf.into())).await
                }
                WsOut::Pong(p) => sink.send(AxumMessage::Pong(p.into())).await,
                WsOut::Ping(p) => sink.send(AxumMessage::Ping(p.into())).await,
                WsOut::Close => {
                    let _ = sink.send(AxumMessage::Close(None)).await;
                    break;
                }
            };
            if res.is_err() {
                break;
            }
        }
    });

    // Reader/state-machine runs here; on return we tear the connection down.
    run_connection(&mut stream, &out_tx, &cancel, &state, id, peer).await;

    // Drop our sender so the writer finishes, then await it.
    drop(out_tx);
    let _ = writer.await;
    cancel.cancel();
}

/// Drive the connection through the state machine
/// `challenge → read_auth → role-specific handshake → authed loop`.
async fn run_connection(
    stream: &mut SplitStream<WebSocket>,
    out_tx: &mpsc::Sender<WsOut>,
    cancel: &CancellationToken,
    state: &AppState,
    id: u64,
    peer: IpAddr,
) {
    // --- challenge: relay speaks first (relay-protocol.md §4) ---
    let mut challenge = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut challenge);
    if out_tx
        .send(WsOut::Frame(RelayFrame {
            frame: Some(Frame::Challenge(proto::Challenge {
                nonce: prost::bytes::Bytes::copy_from_slice(&challenge),
            })),
        }))
        .await
        .is_err()
    {
        return;
    }

    // --- await the auth frame within the challenge timeout ---
    let auth = match read_auth(stream, Duration::from_secs(CHALLENGE_TIMEOUT_SECS)).await {
        AuthRead::Frame(a) => *a,
        AuthRead::Timeout => {
            fail_auth(out_tx, "challenge_timeout", "no auth in time").await;
            return;
        }
        AuthRead::Bad => {
            fail_auth(out_tx, "bad_request", "expected auth frame").await;
            return;
        }
        AuthRead::Closed => return,
    };

    // Dispatch by role (Auth.role oneof — never trust a free-form string).
    let Some(role) = auth.role else {
        fail_auth(out_tx, "bad_request", "missing role in auth").await;
        return;
    };
    match role {
        AuthRole::Agent(a) => {
            auth_agent(stream, out_tx, cancel, state, id, &challenge, auth.signature, a, peer)
                .await;
        }
        AuthRole::Client(c) => {
            auth_client(stream, out_tx, cancel, state, id, &challenge, auth.signature, c, peer)
                .await;
        }
        AuthRole::Pairing(p) => {
            auth_pairing(out_tx, state, &challenge, auth.signature, p, peer).await;
        }
    }
}

/// Pre-auth read loop (relay-protocol.md §4). The only valid pre-auth payload
/// is a single binary frame carrying a `RelayFrame::Auth`. WS-level Ping/Pong
/// are replied to / ignored, Close closes. Anything else → `Bad`.
async fn read_auth(stream: &mut SplitStream<WebSocket>, within: Duration) -> AuthRead {
    let deadline = tokio::time::sleep(within);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return AuthRead::Timeout,
            msg = stream.next() => match msg {
                None | Some(Err(_)) => return AuthRead::Closed,
                // WS-level Ping/Pong are not protobuf; the WS library auto-
                // replies to inbound Pings and we just consume them.
                Some(Ok(AxumMessage::Ping(_))) | Some(Ok(AxumMessage::Pong(_))) => continue,
                Some(Ok(AxumMessage::Close(_))) => return AuthRead::Closed,
                Some(Ok(AxumMessage::Text(_))) => return AuthRead::Bad,
                Some(Ok(AxumMessage::Binary(data))) => {
                    if data.len() > MAX_FRAME_BYTES {
                        return AuthRead::Bad;
                    }
                    return match prost::Message::decode(&data[..]) {
                        Ok(RelayFrame { frame: Some(Frame::Auth(a)) }) => {
                            AuthRead::Frame(Box::new(a))
                        }
                        _ => AuthRead::Bad,
                    };
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// role: agent
// ---------------------------------------------------------------------------

/// Agent handshake (relay-protocol.md §4, §6):
/// verify challenge → upsert namespace → register agent → send AuthOk →
/// replay pending `client_paired` → broadcast `PresenceEvent{ONLINE}` →
/// enter the authed loop. On disconnect, broadcast `PresenceEvent{OFFLINE}`.
#[allow(clippy::too_many_arguments)]
async fn auth_agent(
    stream: &mut SplitStream<WebSocket>,
    out_tx: &mpsc::Sender<WsOut>,
    cancel: &CancellationToken,
    state: &AppState,
    id: u64,
    challenge: &[u8; 32],
    signature: prost::bytes::Bytes,
    agent: proto::AuthAgent,
    peer: IpAddr,
) {
    let Some(agent_pub) = bytes_to_array::<32>(&agent.agent_ed25519_pub) else {
        return fail_auth(out_tx, "bad_request", "agent_ed25519_pub must be 32 bytes").await;
    };
    let Some(sig) = bytes_to_array::<64>(&signature) else {
        return fail_auth(out_tx, "bad_request", "signature must be 64 bytes").await;
    };
    if !verify_challenge(&agent_pub, challenge, &sig) {
        return fail_auth(out_tx, "invalid_signature", "challenge signature").await;
    }
    let (ns_raw, ns) = namespace_id(&agent_pub);

    if let Err(e) = state.store.upsert_namespace(&ns, &agent_pub).await {
        tracing::error!(target: "relay::ws", error = %e, "upsert_namespace failed");
        return fail_auth(out_tx, "bad_request", "internal").await;
    }

    // One agent per namespace: evict the previous connection.
    let handle = ConnHandle {
        id,
        tx: out_tx.clone(),
        cancel: cancel.clone(),
        pubkey: agent_pub,
    };
    if let Some(old) = state.registry.register_agent(&ns, handle) {
        old.cancel.cancel();
    }
    tracing::info!(
        target: "relay::ws",
        role = "agent",
        ns = %short(&ns),
        %peer,
        "authenticated"
    );

    // AuthOk (raw 32-byte namespace_id).
    let _ = out_tx
        .send(WsOut::Frame(RelayFrame {
            frame: Some(Frame::AuthOk(AuthOk {
                namespace_id: prost::bytes::Bytes::copy_from_slice(&ns_raw),
            })),
        }))
        .await;

    // Re-deliver `client_paired` for any pending clients the agent may have
    // missed (the queue lives in DB; same as v1).
    if let Ok(pending) = state.store.list_pending_clients(&ns).await {
        for pc in pending {
            let plat = platform_str_to_i32(&pc.platform);
            let _ = out_tx
                .send(WsOut::Frame(RelayFrame {
                    frame: Some(Frame::ClientPaired(ClientPaired {
                        client_ed25519_pub: prost::bytes::Bytes::copy_from_slice(&pc.ed25519_pub),
                        client_x25519_pub: prost::bytes::Bytes::copy_from_slice(&pc.x25519_pub),
                        platform: plat.unwrap_or(proto::Platform::Unspecified as i32),
                    })),
                }))
                .await;
        }
    }

    // Tell the other (already-connected) members that this agent is now ONLINE.
    let _ = state.registry.broadcast_ns(
        &ns,
        presence_frame(agent_pub, proto::Status::Online as i32),
        Some(id),
    );

    authed_loop(stream, out_tx, cancel, state, Role::Agent, &ns, agent_pub).await;

    // Disconnect cleanup. `remove_agent` is identity-checked, so a no-op if we
    // were already replaced. Always broadcast OFFLINE per the spec
    // (relay-protocol.md §4: idempotent presence, downstream handles repeats).
    state.registry.remove_agent(&ns, id);
    let _ = state.registry.broadcast_ns(
        &ns,
        presence_frame(agent_pub, proto::Status::Offline as i32),
        None,
    );
    tracing::info!(target: "relay::ws", role = "agent", ns = %short(&ns), "disconnected");
}

// ---------------------------------------------------------------------------
// role: pairing (short-lived: AuthOk then Close)
// ---------------------------------------------------------------------------

/// Pairing handshake (relay-protocol.md §7): verify challenge → namespace must
/// exist → consume the single-use token → upsert pending client → notify the
/// agent (if connected) → send AuthOk → close.
#[allow(clippy::too_many_arguments)]
async fn auth_pairing(
    out_tx: &mpsc::Sender<WsOut>,
    state: &AppState,
    challenge: &[u8; 32],
    signature: prost::bytes::Bytes,
    p: proto::AuthPairing,
    peer: IpAddr,
) {
    let Some(client_pub) = bytes_to_array::<32>(&p.client_ed25519_pub) else {
        return fail_auth(out_tx, "bad_request", "client_ed25519_pub must be 32 bytes").await;
    };
    let Some(sig) = bytes_to_array::<64>(&signature) else {
        return fail_auth(out_tx, "bad_request", "signature must be 64 bytes").await;
    };
    if !verify_challenge(&client_pub, challenge, &sig) {
        return fail_auth(out_tx, "invalid_signature", "challenge signature").await;
    }
    let Some(ns_bytes) = bytes_to_array::<32>(&p.namespace_id) else {
        return fail_auth(out_tx, "bad_request", "namespace_id must be 32 bytes").await;
    };
    let ns = hex::encode(ns_bytes);
    let Some(client_x) = bytes_to_array::<32>(&p.client_x25519_pub) else {
        return fail_auth(out_tx, "bad_request", "client_x25519_pub must be 32 bytes").await;
    };
    let Some(token) = bytes_to_array::<32>(&p.pairing_token) else {
        return fail_auth(out_tx, "bad_request", "pairing_token must be 32 bytes").await;
    };
    let Some(plat) = i32_to_internal_platform(p.platform) else {
        return fail_auth(out_tx, "bad_request", "platform unspecified").await;
    };

    match state.store.namespace_exists(&ns).await {
        Ok(true) => {}
        Ok(false) => return fail_auth(out_tx, "not_found", "namespace").await,
        Err(e) => {
            tracing::error!(target: "relay::ws", error = %e, "namespace_exists failed");
            return fail_auth(out_tx, "bad_request", "internal").await;
        }
    }
    match state.store.consume_pairing_token(&ns, &token).await {
        Ok(true) => {}
        Ok(false) => return fail_auth(out_tx, "pairing_closed", "token").await,
        Err(e) => {
            tracing::error!(target: "relay::ws", error = %e, "consume_pairing_token failed");
            return fail_auth(out_tx, "bad_request", "internal").await;
        }
    }

    if let Err(e) = state
        .store
        .upsert_pending_client(&ns, &client_pub, &client_x, &p.device_token, plat.as_str())
        .await
    {
        tracing::error!(target: "relay::ws", error = %e, "upsert_pending_client failed");
        return fail_auth(out_tx, "bad_request", "internal").await;
    }

    // Notify the agent (if connected) that a new device paired.
    if let Some(atx) = state.registry.agent_tx(&ns) {
        let _ = atx
            .send(WsOut::Frame(RelayFrame {
                frame: Some(Frame::ClientPaired(ClientPaired {
                    client_ed25519_pub: prost::bytes::Bytes::copy_from_slice(&client_pub),
                    client_x25519_pub: prost::bytes::Bytes::copy_from_slice(&client_x),
                    platform: proto::Platform::from(plat) as i32,
                })),
            }))
            .await;
    }

    tracing::info!(
        target: "relay::ws",
        role = "pairing",
        ns = %short(&ns),
        %peer,
        "paired (pending)"
    );

    // AuthOk with the raw namespace_id, then close. Pairing is short-lived
    // (relay-protocol.md §7).
    let _ = out_tx
        .send(WsOut::Frame(RelayFrame {
            frame: Some(Frame::AuthOk(AuthOk {
                namespace_id: prost::bytes::Bytes::copy_from_slice(&ns_bytes),
            })),
        }))
        .await;
    let _ = out_tx.send(WsOut::Close).await;
}

// ---------------------------------------------------------------------------
// role: client
// ---------------------------------------------------------------------------

/// Client handshake (relay-protocol.md §5, §6):
/// verify challenge → namespace exists → client is authorized → refresh
/// device_token → register client → send AuthOk → drain queued messages →
/// broadcast `PresenceEvent{ONLINE}` → enter the authed loop. On disconnect,
/// broadcast `PresenceEvent{OFFLINE}`.
#[allow(clippy::too_many_arguments)]
async fn auth_client(
    stream: &mut SplitStream<WebSocket>,
    out_tx: &mpsc::Sender<WsOut>,
    cancel: &CancellationToken,
    state: &AppState,
    id: u64,
    challenge: &[u8; 32],
    signature: prost::bytes::Bytes,
    c: proto::AuthClient,
    peer: IpAddr,
) {
    let Some(client_pub) = bytes_to_array::<32>(&c.client_ed25519_pub) else {
        return fail_auth(out_tx, "bad_request", "client_ed25519_pub must be 32 bytes").await;
    };
    let Some(sig) = bytes_to_array::<64>(&signature) else {
        return fail_auth(out_tx, "bad_request", "signature must be 64 bytes").await;
    };
    if !verify_challenge(&client_pub, challenge, &sig) {
        return fail_auth(out_tx, "invalid_signature", "challenge signature").await;
    }
    let Some(ns_bytes) = bytes_to_array::<32>(&c.namespace_id) else {
        return fail_auth(out_tx, "bad_request", "namespace_id must be 32 bytes").await;
    };
    let ns = hex::encode(ns_bytes);
    // Platform enum is required (no UNSPECIFIED). The DB stores it as
    // "ios"/"android" — we still validate the wire value here.
    if i32_to_internal_platform(c.platform).is_none() {
        return fail_auth(out_tx, "bad_request", "platform unspecified").await;
    }

    match state.store.namespace_exists(&ns).await {
        Ok(true) => {}
        Ok(false) => return fail_auth(out_tx, "not_found", "namespace").await,
        Err(e) => {
            tracing::error!(target: "relay::ws", error = %e, "namespace_exists failed");
            return fail_auth(out_tx, "bad_request", "internal").await;
        }
    }
    match state.store.is_authorized_client(&ns, &client_pub).await {
        Ok(true) => {}
        Ok(false) => return fail_auth(out_tx, "unauthorized", "client").await,
        Err(e) => {
            tracing::error!(target: "relay::ws", error = %e, "is_authorized_client failed");
            return fail_auth(out_tx, "bad_request", "internal").await;
        }
    }

    // Push tokens rotate: refresh on each connect.
    let _ = state
        .store
        .update_client_device_token(&ns, &client_pub, &c.device_token)
        .await;

    let pub_hex = hex::encode(client_pub);
    let handle = ConnHandle {
        id,
        tx: out_tx.clone(),
        cancel: cancel.clone(),
        pubkey: client_pub,
    };
    if let Some(old) = state.registry.register_client(&ns, &pub_hex, handle) {
        old.cancel.cancel();
    }
    tracing::info!(
        target: "relay::ws",
        role = "client",
        ns = %short(&ns),
        %peer,
        "authenticated"
    );

    // AuthOk.
    let _ = out_tx
        .send(WsOut::Frame(RelayFrame {
            frame: Some(Frame::AuthOk(AuthOk {
                namespace_id: prost::bytes::Bytes::copy_from_slice(&ns_bytes),
            })),
        }))
        .await;

    // Drain anything queued while offline (FIFO, relay-protocol.md §5.2).
    deliver_pending(out_tx, &state.store, &ns, client_pub).await;

    // Broadcast ONLINE to the other members of the namespace.
    let _ = state.registry.broadcast_ns(
        &ns,
        presence_frame(client_pub, proto::Status::Online as i32),
        Some(id),
    );

    authed_loop(stream, out_tx, cancel, state, Role::Client, &ns, client_pub).await;

    // Disconnect cleanup.
    state.registry.remove_client(&ns, &pub_hex, id);
    let _ = state.registry.broadcast_ns(
        &ns,
        presence_frame(client_pub, proto::Status::Offline as i32),
        None,
    );
    tracing::info!(
        target: "relay::ws",
        role = "client",
        ns = %short(&ns),
        "disconnected"
    );
}

// ---------------------------------------------------------------------------
// Authenticated loop (shared by agent + client)
// ---------------------------------------------------------------------------

/// Authenticated state machine (relay-protocol.md §4-8). Reads frames, enforces
/// the v2 size cap (`MAX_FRAME_BYTES` default, `MAX_LIVE_FRAME_BYTES` for
/// `Message{live:true}`), dispatches each frame to [`handle_inbound`], and
/// fires the WS-level keepalive ping every `PING_INTERVAL_SECS`.
async fn authed_loop(
    stream: &mut SplitStream<WebSocket>,
    out_tx: &mpsc::Sender<WsOut>,
    cancel: &CancellationToken,
    state: &AppState,
    role: Role,
    ns: &str,
    my_pub: [u8; 32],
) {
    let mut ping = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seen = Instant::now();
    let mut rate = limits::ConnRate::new();

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = out_tx.send(WsOut::Close).await;
                break;
            }
            _ = ping.tick() => {
                if last_seen.elapsed() > Duration::from_secs(IDLE_TIMEOUT_SECS) {
                    tracing::info!(
                        target: "relay::ws",
                        role = ?role,
                        ns = %short(ns),
                        "idle timeout; closing"
                    );
                    let _ = out_tx.send(WsOut::Close).await;
                    break;
                }
                if out_tx.send(WsOut::Ping(Vec::new())).await.is_err() {
                    break;
                }
            }
            msg = stream.next() => {
                let Some(Ok(m)) = msg else { break };
                last_seen = Instant::now();
                match m {
                    AxumMessage::Binary(data) => {
                        // Two-pass: decode first to discover the
                        // `Message.live` flag, then enforce the right cap on
                        // the raw inbound bytes. The cap is per WS frame
                        // (relay-protocol.md §5) — bytes on the wire, not
                        // post-decode size.
                        let frame: RelayFrame = match prost::Message::decode(&data[..]) {
                            Ok(f) => f,
                            Err(_) => {
                                let _ = out_tx.send(WsOut::Frame(error_frame(
                                    "bad_request",
                                    "malformed protobuf",
                                ))).await;
                                let _ = out_tx.send(WsOut::Close).await;
                                return;
                            }
                        };
                        let is_live_msg = matches!(
                            &frame.frame,
                            Some(Frame::Message(m)) if m.live
                        );
                        let cap = if is_live_msg {
                            MAX_LIVE_FRAME_BYTES
                        } else {
                            MAX_FRAME_BYTES
                        };
                        if data.len() > cap {
                            let _ = out_tx.send(WsOut::Frame(error_frame(
                                "payload_too_large",
                                "frame exceeds limit",
                            ))).await;
                            let _ = out_tx.send(WsOut::Close).await;
                            return;
                        }
                        match handle_inbound(
                            out_tx, state, role, ns, &my_pub, &mut rate, frame,
                        ).await {
                            Flow::Continue => {}
                            Flow::Close => return,
                        }
                    }
                    AxumMessage::Ping(p) => {
                        let _ = out_tx.send(WsOut::Pong(p.to_vec())).await;
                    }
                    AxumMessage::Pong(_) => {} // activity already recorded
                    AxumMessage::Text(_) => {
                        // v2 is binary-only; ignore text frames (forward-compat).
                    }
                    AxumMessage::Close(_) => break,
                }
            }
        }
    }
    // Keep the namespace alive timestamp fresh on clean disconnect.
    let _ = state.store.touch_namespace(ns).await;
}

/// Parse and dispatch one decoded, size-validated post-auth `RelayFrame`.
#[allow(clippy::too_many_arguments)]
async fn handle_inbound(
    out_tx: &mpsc::Sender<WsOut>,
    state: &AppState,
    role: Role,
    ns: &str,
    my_pub: &[u8; 32],
    rate: &mut limits::ConnRate,
    frame: RelayFrame,
) -> Flow {
    let Some(f) = frame.frame else {
        let _ = out_tx
            .send(WsOut::Frame(error_frame("bad_request", "empty frame")))
            .await;
        return Flow::Continue;
    };
    match f {
        Frame::Message(m) => {
            if !rate.allow_message() {
                let _ = out_tx
                    .send(WsOut::Frame(error_frame("rate_limited", "too many messages")))
                    .await;
                let _ = out_tx.send(WsOut::Close).await;
                return Flow::Close;
            }
            if let Err(e) = forward_message(out_tx, state, ns, my_pub, m).await {
                tracing::warn!(target: "relay::ws", error = %e, "forward_message failed");
            }
        }
        Frame::Authorize(a) => {
            if role != Role::Agent {
                let _ = out_tx
                    .send(WsOut::Frame(error_frame(
                        "bad_request",
                        "frame not allowed for role",
                    )))
                    .await;
            } else if let Err(e) = handle_authorize(out_tx, state, ns, a).await {
                tracing::warn!(target: "relay::ws", error = %e, "handle_authorize failed");
            }
        }
        Frame::PairingStart(p) => {
            if role != Role::Agent {
                let _ = out_tx
                    .send(WsOut::Frame(error_frame(
                        "bad_request",
                        "frame not allowed for role",
                    )))
                    .await;
            } else if let Err(e) = handle_pairing_start(out_tx, state, ns, p).await {
                tracing::warn!(target: "relay::ws", error = %e, "handle_pairing_start failed");
            }
        }
        Frame::PairingStop(_) => {
            if role != Role::Agent {
                let _ = out_tx
                    .send(WsOut::Frame(error_frame(
                        "bad_request",
                        "frame not allowed for role",
                    )))
                    .await;
            } else if let Err(e) = state.store.pairing_stop(ns).await {
                tracing::warn!(target: "relay::ws", error = %e, "pairing_stop failed");
            } else {
                let _ = out_tx
                    .send(WsOut::Frame(RelayFrame {
                        frame: Some(Frame::PairingStopOk(PairingStopOk {})),
                    }))
                    .await;
            }
        }
        Frame::PresenceRequest(_) => {
            // PresenceRequest is allowed for both roles — respond to the
            // requester only with the namespace's currently-online pubkeys
            // (relay-protocol.md §4). Counted against the same per-connection
            // budget as messages (§9) so it can't be spammed to lock the
            // registry on every frame.
            if !rate.allow_message() {
                let _ = out_tx
                    .send(WsOut::Frame(error_frame("rate_limited", "too many requests")))
                    .await;
                let _ = out_tx.send(WsOut::Close).await;
                return Flow::Close;
            }
            let online = state.registry.list_online(ns);
            let list = PresenceList {
                online: online
                    .iter()
                    .map(|k| prost::bytes::Bytes::copy_from_slice(k))
                    .collect(),
            };
            let _ = out_tx
                .send(WsOut::Frame(RelayFrame {
                    frame: Some(Frame::PresenceList(list)),
                }))
                .await;
        }
        Frame::Error(_) => {
            // Forward-compat: ignore inbound `Error` (no protocol-defined
            // reaction; relay may log it).
        }
        // Server-to-client frames a peer should not be sending post-auth.
        Frame::Auth(_)
        | Frame::Challenge(_)
        | Frame::AuthOk(_)
        | Frame::AuthError(_)
        | Frame::AuthorizeOk(_)
        | Frame::PairingReady(_)
        | Frame::PairingStopOk(_)
        | Frame::ClientPaired(_)
        | Frame::PeerOffline(_)
        | Frame::PresenceList(_)
        | Frame::PresenceEvent(_) => {
            tracing::debug!(
                target: "relay::ws",
                "ignoring server-to-client frame from peer"
            );
        }
    }
    Flow::Continue
}

// ---------------------------------------------------------------------------
// Message routing
// ---------------------------------------------------------------------------

/// Route an end-to-end `Message` to its recipient
/// (relay-protocol.md §5.2). The relay rewrites `peer` from the destination
/// (inbound) to the authenticated sender's pubkey (outbound) — exactly like v1.
/// Live messages are route-or-fail: if the recipient is offline, return
/// `PeerOffline` to the sender (relay-protocol.md §3). Store-and-forward
/// messages enqueue (capped at `QUEUE_MAX_PER_DEST`) and, for clients with a
/// device_token, fire a push.
#[allow(clippy::too_many_arguments)]
async fn forward_message(
    out_tx: &mpsc::Sender<WsOut>,
    state: &AppState,
    ns: &str,
    sender_pub: &[u8; 32],
    m: ProtoMessage,
) -> anyhow::Result<()> {
    // Validate the lengths of peer (32B) and nonce (12B) and copy the raw
    // bytes out. Anything else is a protocol violation from the sender.
    let to: [u8; 32] = match m.peer.len().cmp(&32) {
        Ordering::Equal => {
            let mut b = [0u8; 32];
            b.copy_from_slice(m.peer.as_ref());
            b
        }
        _ => {
            let _ = out_tx
                .send(WsOut::Frame(error_frame("bad_request", "peer must be 32 bytes")))
                .await;
            return Ok(());
        }
    };
    let nonce: [u8; 12] = match m.nonce.len().cmp(&12) {
        Ordering::Equal => {
            let mut b = [0u8; 12];
            b.copy_from_slice(m.nonce.as_ref());
            b
        }
        _ => {
            let _ = out_tx
                .send(WsOut::Frame(error_frame("bad_request", "nonce must be 12 bytes")))
                .await;
            return Ok(());
        }
    };
    let ciphertext = m.ciphertext.to_vec();

    // Resolve the recipient within the namespace.
    let agent_pub = state.store.agent_pub(ns).await?;
    let is_agent_dest = agent_pub.as_ref() == Some(&to);
    let is_client_dest =
        !is_agent_dest && state.store.is_authorized_client(ns, &to).await?;
    if !is_agent_dest && !is_client_dest {
        let _ = out_tx
            .send(WsOut::Frame(error_frame("not_found", "recipient")))
            .await;
        return Ok(());
    }

    // Build outbound Message: peer = sender_pub (relay-protocol.md §5.2).
    let out_msg = ProtoMessage {
        ciphertext: prost::bytes::Bytes::copy_from_slice(&ciphertext),
        nonce: prost::bytes::Bytes::copy_from_slice(&nonce),
        peer: prost::bytes::Bytes::copy_from_slice(sender_pub),
        live: false,
    };
    let out_frame = RelayFrame {
        frame: Some(Frame::Message(out_msg)),
    };

    // Try live delivery first.
    let live_tx = if is_agent_dest {
        state.registry.agent_tx(ns)
    } else {
        state.registry.client_tx(ns, &hex::encode(to))
    };
    if let Some(tx) = live_tx
        && tx.send(WsOut::Frame(out_frame.clone())).await.is_ok()
    {
        return Ok(());
    }
    // writer gone: fall through.

    // Live: route-or-fail. Do NOT enqueue, do NOT push.
    if m.live {
        let offline = RelayFrame {
            frame: Some(Frame::PeerOffline(PeerOffline {
                peer: prost::bytes::Bytes::copy_from_slice(&to),
            })),
        };
        let _ = out_tx.send(WsOut::Frame(offline)).await;
        return Ok(());
    }

    // Store-and-forward: enqueue, then push if the recipient is a client
    // (APNs/FCM — push.rs builds the platform-specific payload).
    let ok = state
        .store
        .enqueue(ns, &to, sender_pub, &nonce, &ciphertext, QUEUE_MAX_PER_DEST)
        .await?;
    if !ok {
        let _ = out_tx
            .send(WsOut::Frame(error_frame("queue_full", "recipient queue full")))
            .await;
        return Ok(());
    }

    if is_client_dest
        && let Some(client) = state.store.get_client(ns, &to).await?
        && let Some(dt) = client.device_token
        // An empty token means the device never registered one (or connected
        // before APNs/FCM registration finished). Pushing with it yields a
        // `MissingDeviceToken` from Apple, so skip — the queued message is
        // still drained when the device next opens a WS.
        && !dt.is_empty()
        && let Some(plat) = Platform::parse(&client.platform)
    {
        let item = PushItem {
            namespace_id: ns.to_string(),
            from_hex: hex::encode(sender_pub),
            nonce_hex: hex::encode(nonce),
            ciphertext,
        };
        state.pusher.notify(&dt, plat, &item).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Agent-only handlers
// ---------------------------------------------------------------------------

/// `Authorize` (relay-protocol.md §6): replace-semantics on the client's
/// authorized set; evict any live, now-revoked clients; broadcast
/// `PresenceEvent{OFFLINE}` for each.
async fn handle_authorize(
    out_tx: &mpsc::Sender<WsOut>,
    state: &AppState,
    ns: &str,
    a: Authorize,
) -> anyhow::Result<()> {
    let mut keys: Vec<[u8; 32]> = Vec::with_capacity(a.clients.len());
    for k in &a.clients {
        let Some(b) = bytes_to_array::<32>(k) else {
            let _ = out_tx
                .send(WsOut::Frame(error_frame("bad_request", "client pubkey must be 32 bytes")))
                .await;
            return Ok(());
        };
        keys.push(b);
    }
    let (count, revoked) = state.store.apply_authorize(ns, &keys).await?;
    for r in revoked {
        if let Some(old) = state.registry.evict_client(ns, &hex::encode(r)) {
            old.cancel.cancel();
        }
        // Best-effort: the revoked client is being kicked, so its peers
        // learn it's gone.
        let _ = state.registry.broadcast_ns(
            ns,
            presence_frame(r, proto::Status::Offline as i32),
            None,
        );
    }
    let _ = out_tx
        .send(WsOut::Frame(RelayFrame {
            frame: Some(Frame::AuthorizeOk(AuthorizeOk {
                authorized: count as u32,
            })),
        }))
        .await;
    Ok(())
}

/// `PairingStart` (relay-protocol.md §7): open the pairing window with a
/// single-use token. `ttl` is clamped to `[1, PAIRING_TTL_MAX]` seconds.
async fn handle_pairing_start(
    out_tx: &mpsc::Sender<WsOut>,
    state: &AppState,
    ns: &str,
    p: PairingStart,
) -> anyhow::Result<()> {
    let Some(token) = bytes_to_array::<32>(&p.pairing_token) else {
        let _ = out_tx
            .send(WsOut::Frame(error_frame("bad_request", "pairing_token must be 32 bytes")))
            .await;
        return Ok(());
    };
    let ttl = p.ttl.clamp(1, PAIRING_TTL_MAX as u32);
    let expiry = now_ms() + (ttl as i64) * 1000;
    state.store.pairing_start(ns, &token, expiry).await?;
    let _ = out_tx
        .send(WsOut::Frame(RelayFrame {
            frame: Some(Frame::PairingReady(PairingReady { ttl })),
        }))
        .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Queue draining
// ---------------------------------------------------------------------------

/// Drain the recipient's queue in FIFO order, deleting each row after delivery
/// (relay-protocol.md §5.2). A peer that disconnects mid-drain leaves the
/// rest of the queue intact for the next reconnect.
async fn deliver_pending(
    out_tx: &mpsc::Sender<WsOut>,
    store: &crate::store::Store,
    ns: &str,
    to_pub: [u8; 32],
) {
    let pending = match store.fetch_pending(ns, &to_pub).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(target: "relay::ws", error = %e, "fetch_pending failed");
            return;
        }
    };
    for qm in pending {
        let frame = RelayFrame {
            frame: Some(Frame::Message(ProtoMessage {
                ciphertext: prost::bytes::Bytes::copy_from_slice(&qm.ciphertext),
                nonce: prost::bytes::Bytes::copy_from_slice(&qm.nonce),
                peer: prost::bytes::Bytes::copy_from_slice(&qm.from_pub),
                live: false,
            })),
        };
        if out_tx.send(WsOut::Frame(frame)).await.is_err() {
            return; // peer gone; leave the rest queued
        }
        if let Err(e) = store.delete_pending(qm.id).await {
            tracing::error!(target: "relay::ws", error = %e, "delete_pending failed");
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `RelayFrame{Error{code,message}}` for control-plane errors.
fn error_frame(code: &str, message: &str) -> RelayFrame {
    RelayFrame {
        frame: Some(Frame::Error(proto::Error {
            code: code.to_string(),
            message: message.to_string(),
        })),
    }
}

/// Build a `RelayFrame{PresenceEvent{pubkey,status}}`.
fn presence_frame(pubkey: [u8; 32], status: i32) -> RelayFrame {
    RelayFrame {
        frame: Some(Frame::PresenceEvent(PresenceEvent {
            pubkey: prost::bytes::Bytes::copy_from_slice(&pubkey),
            status,
        })),
    }
}

/// Send `AuthError{code,message}` and a Close, used on every pre-auth failure
/// (relay-protocol.md §4 + §11).
async fn fail_auth(out_tx: &mpsc::Sender<WsOut>, code: &str, message: &str) {
    let _ = out_tx
        .send(WsOut::Frame(RelayFrame {
            frame: Some(Frame::AuthError(AuthError {
                code: code.to_string(),
                message: message.to_string(),
            })),
        }))
        .await;
    let _ = out_tx.send(WsOut::Close).await;
}

/// Try to view `b` as a fixed-size array of `N` bytes. Returns `None` on
/// length mismatch — used to validate every `bytes` field whose length the
/// spec pins (pubkeys 32B, signature 64B, …).
fn bytes_to_array<const N: usize>(b: &prost::bytes::Bytes) -> Option<[u8; N]> {
    if b.len() != N {
        return None;
    }
    let mut out = [0u8; N];
    out.copy_from_slice(b.as_ref());
    Some(out)
}

/// Map our internal `push::Platform` (as a `&str` from the DB column) to the
/// protobuf enum's wire value (1 = iOS, 2 = Android).
fn platform_str_to_i32(s: &str) -> Option<i32> {
    match s {
        "ios" => Some(proto::Platform::Ios as i32),
        "android" => Some(proto::Platform::Android as i32),
        _ => None,
    }
}

/// Inverse of [`platform_str_to_i32`]: protobuf wire value → `push::Platform`.
fn i32_to_internal_platform(v: i32) -> Option<Platform> {
    if v == proto::Platform::Ios as i32 {
        Some(Platform::Ios)
    } else if v == proto::Platform::Android as i32 {
        Some(Platform::Android)
    } else {
        None
    }
}

/// Bridge `push::Platform` → protobuf wire value (used when forwarding
/// `ClientPaired` to the agent after a successful pairing).
impl From<Platform> for proto::Platform {
    fn from(p: Platform) -> Self {
        match p {
            Platform::Ios => proto::Platform::Ios,
            Platform::Android => proto::Platform::Android,
        }
    }
}

/// Truncate a namespace_id / pubkey for logging.
fn short(s: &str) -> String {
    let n = s.len().min(8);
    format!("{}…", &s[..n])
}

// ---------------------------------------------------------------------------
// Unit tests (non-network helpers)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `presence_frame` round-trips through `prost` encode/decode with the
    /// exact bytes we expect on the wire (relay-protocol.md §4).
    #[test]
    fn presence_frame_round_trip() {
        let pubkey = [0xAAu8; 32];
        let frame = presence_frame(pubkey, proto::Status::Online as i32);
        let bytes = prost::Message::encode_to_vec(&frame);
        let decoded: RelayFrame = prost::Message::decode(&bytes[..]).expect("decode");
        match decoded.frame {
            Some(Frame::PresenceEvent(PresenceEvent { pubkey: pk, status })) => {
                assert_eq!(pk.as_ref(), &pubkey[..]);
                assert_eq!(status, proto::Status::Online as i32);
            }
            other => panic!("expected PresenceEvent, got {other:?}"),
        }
    }

    /// `error_frame` round-trips with the exact code/message we passed.
    #[test]
    fn error_frame_round_trip() {
        let frame = error_frame("bad_request", "missing field");
        let bytes = prost::Message::encode_to_vec(&frame);
        let decoded: RelayFrame = prost::Message::decode(&bytes[..]).expect("decode");
        match decoded.frame {
            Some(Frame::Error(proto::Error { code, message })) => {
                assert_eq!(code, "bad_request");
                assert_eq!(message, "missing field");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// `AuthOk{namespace_id}` round-trips; the raw 32B namespace_id must be
    /// preserved byte-for-byte.
    #[test]
    fn authok_round_trip() {
        let ns_raw = [0x77u8; 32];
        let frame = RelayFrame {
            frame: Some(Frame::AuthOk(AuthOk {
                namespace_id: prost::bytes::Bytes::copy_from_slice(&ns_raw),
            })),
        };
        let bytes = prost::Message::encode_to_vec(&frame);
        let decoded: RelayFrame = prost::Message::decode(&bytes[..]).expect("decode");
        match decoded.frame {
            Some(Frame::AuthOk(AuthOk { namespace_id })) => {
                assert_eq!(namespace_id.as_ref(), &ns_raw[..]);
            }
            other => panic!("expected AuthOk, got {other:?}"),
        }
    }

    /// `bytes_to_array` accepts the exact length and rejects the rest.
    #[test]
    fn bytes_to_array_validates_length() {
        let good = prost::bytes::Bytes::from_static(&[0xAB; 32]);
        assert_eq!(bytes_to_array::<32>(&good), Some([0xAB; 32]));

        let too_short = prost::bytes::Bytes::from_static(&[0xAB; 31]);
        assert_eq!(bytes_to_array::<32>(&too_short), None);

        let too_long = prost::bytes::Bytes::from_static(&[0xAB; 33]);
        assert_eq!(bytes_to_array::<32>(&too_long), None);

        let empty = prost::bytes::Bytes::new();
        assert_eq!(bytes_to_array::<64>(&empty), None);
    }

    /// Platform conversion is total on the wire values we accept.
    #[test]
    fn platform_conversion() {
        assert_eq!(i32_to_internal_platform(0), None);
        assert_eq!(i32_to_internal_platform(1), Some(Platform::Ios));
        assert_eq!(i32_to_internal_platform(2), Some(Platform::Android));
        assert_eq!(i32_to_internal_platform(99), None);

        assert_eq!(platform_str_to_i32("ios"), Some(1));
        assert_eq!(platform_str_to_i32("android"), Some(2));
        assert_eq!(platform_str_to_i32("linux"), None);
    }
}
