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
//! whole loop is cancellable on stop.

use std::sync::Arc;
use std::time::Duration;

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

/// Run the reconnecting WS loop until `cancel` fires (relay-protocol.md §8).
pub(crate) async fn run_loop(
    state: Arc<RelayState>,
    mut outbound_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    cancel: CancellationToken,
) {
    let mut backoff_step: u32 = 0;
    loop {
        if cancel.is_cancelled() {
            return;
        }

        match connect_once(&state, &mut outbound_rx, &cancel).await {
            Ok(()) => {
                // Clean disconnect (cancelled or graceful): reset backoff.
                backoff_step = 0;
            }
            Err(e) => {
                warn!(crate_name = "skald-relay-client", error = %e, "relay connection ended");
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

    // 5. Main dispatch loop: outbound queue, inbound frames, WS-level Ping/Pong.
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = sink.send(WsMessage::Close(None)).await;
                return Ok(());
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
