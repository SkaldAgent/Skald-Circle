//! End-to-end integration test for `skald-relay-client` against the **real**
//! relay server (`skald-relay-server`) booted in-process on an ephemeral port.
//!
//! It drives the full agent-role flow through the public `RelayClient` API while
//! a hand-rolled "mobile client" (raw `tokio-tungstenite` speaking v2 protobuf +
//! the shared E2E crypto) plays the counterpart:
//!
//!   start → Connected → start_pairing → pair → ClientPaired → authorize →
//!   send (mobile decrypts) → mobile reply → Message → replay dropped →
//!   revoke → ClientRevoked.
//!
//! This exercises the persist-before-seal counter path, the nonce direction /
//! AAD construction, the pairing window, and the events channel end to end.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use skald_relay_common::crypto::{self, DIR_CLIENT_TO_AGENT};
use skald_relay_common::proto::v2::{
    self, Auth, AuthClient, AuthPairing, Message as ProtoMessage, RelayFrame,
};
use skald_relay_common::proto::v2::auth::Role as AuthRole;
use skald_relay_common::proto::v2::relay_frame::Frame;
use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use skald_relay_client::{RelayClient, RelayClientConfig, RelayEvent, SeedSource};
use skald_relay_server::config::Config;
use skald_relay_server::{AppState, router};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos}-{}-{seq}", std::process::id())
}

/// Boot a relay on a random port with a throwaway SQLite file. Returns its addr.
async fn spawn_relay() -> SocketAddr {
    let db = std::env::temp_dir().join(format!("relay-srv-{}.db", unique_suffix()));
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        db_path: db.to_string_lossy().into(),
        pipe: skald_relay_server::config::PipeConfig::default(),
    };
    let state = AppState::build(cfg).await.expect("build relay state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    addr
}

/// A fresh on-disk SQLite pool for the client (a temp-file DB, not `:memory:`,
/// so the WS-loop task and the test task share the same database across the
/// pool's connections).
async fn client_pool() -> Arc<SqlitePool> {
    let path = std::env::temp_dir().join(format!("relay-cli-{}.db", unique_suffix()));
    let _ = std::fs::remove_file(&path);
    let url = format!("sqlite://{}?mode=rwc", path.display());
    Arc::new(SqlitePool::connect(&url).await.expect("client pool"))
}

// ── raw WS helpers (mobile side) ────────────────────────────────────────────

async fn connect(addr: SocketAddr) -> Ws {
    let url = format!("ws://{addr}/v1/ws");
    let (ws, _) = tokio_tungstenite::connect_async(url).await.expect("connect");
    ws
}

async fn send(ws: &mut Ws, frame: &RelayFrame) {
    ws.send(WsMessage::Binary(frame.encode_to_vec().into()))
        .await
        .expect("send binary");
}

async fn recv(ws: &mut Ws) -> RelayFrame {
    loop {
        let m = ws.next().await.expect("stream open").expect("ws frame");
        match m {
            WsMessage::Binary(b) => return RelayFrame::decode(b.as_ref()).expect("decode"),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            WsMessage::Close(f) => panic!("unexpected ws close: {f:?}"),
            other => panic!("unexpected ws frame: {other:?}"),
        }
    }
}

async fn read_challenge(ws: &mut Ws) -> [u8; 32] {
    match recv(ws).await.frame {
        Some(Frame::Challenge(c)) => c.nonce.as_ref().try_into().expect("32B challenge"),
        other => panic!("expected Challenge, got {other:?}"),
    }
}

fn auth_pairing_frame(
    sk: &SigningKey,
    challenge: &[u8; 32],
    ns_raw: &[u8; 32],
    token: &[u8; 32],
    x25519_pub: &[u8; 32],
) -> RelayFrame {
    let sig = crypto::sign_challenge(sk, challenge);
    RelayFrame {
        frame: Some(Frame::Auth(Auth {
            signature: Bytes::copy_from_slice(&sig),
            role: Some(AuthRole::Pairing(AuthPairing {
                namespace_id: Bytes::copy_from_slice(ns_raw),
                client_ed25519_pub: Bytes::copy_from_slice(&sk.verifying_key().to_bytes()),
                client_x25519_pub: Bytes::copy_from_slice(x25519_pub),
                pairing_token: Bytes::copy_from_slice(token),
                device_token: "devtok".into(),
                platform: v2::Platform::Ios as i32,
            })),
        })),
    }
}

fn auth_client_frame(sk: &SigningKey, challenge: &[u8; 32], ns_raw: &[u8; 32]) -> RelayFrame {
    let sig = crypto::sign_challenge(sk, challenge);
    RelayFrame {
        frame: Some(Frame::Auth(Auth {
            signature: Bytes::copy_from_slice(&sig),
            role: Some(AuthRole::Client(AuthClient {
                namespace_id: Bytes::copy_from_slice(ns_raw),
                client_ed25519_pub: Bytes::copy_from_slice(&sk.verifying_key().to_bytes()),
                device_token: "devtok".into(),
                platform: v2::Platform::Ios as i32,
            })),
        })),
    }
}

/// Pair on a short-lived side connection (challenge → auth(pairing) → AuthOk).
async fn pair(addr: SocketAddr, sk: &SigningKey, ns_raw: &[u8; 32], token: &[u8; 32], x_pub: &[u8; 32]) {
    let mut ws = connect(addr).await;
    let c = read_challenge(&mut ws).await;
    send(&mut ws, &auth_pairing_frame(sk, &c, ns_raw, token, x_pub)).await;
    match recv(&mut ws).await.frame {
        Some(Frame::AuthOk(_)) => {}
        other => panic!("pairing expected AuthOk, got {other:?}"),
    }
    drop(ws);
}

/// Connect as the authorized client role; returns the live socket.
async fn auth_client(addr: SocketAddr, sk: &SigningKey, ns_raw: &[u8; 32]) -> Ws {
    let mut ws = connect(addr).await;
    let c = read_challenge(&mut ws).await;
    send(&mut ws, &auth_client_frame(sk, &c, ns_raw)).await;
    match recv(&mut ws).await.frame {
        Some(Frame::AuthOk(_)) => {}
        other => panic!("client expected AuthOk, got {other:?}"),
    }
    ws
}

// ── event helpers ───────────────────────────────────────────────────────────

async fn next_event(rx: &mut broadcast::Receiver<RelayEvent>) -> RelayEvent {
    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timed out waiting for event")
        .expect("event recv")
}

/// Next event that is not a `Connected`/`Disconnected` heartbeat.
async fn next_significant(rx: &mut broadcast::Receiver<RelayEvent>) -> RelayEvent {
    loop {
        match next_event(rx).await {
            RelayEvent::Connected | RelayEvent::Disconnected => continue,
            other => return other,
        }
    }
}

#[tokio::test]
async fn full_round_trip() {
    let addr = spawn_relay().await;

    // Build the client (agent role) pointed at the in-process relay.
    let pool = client_pool().await;
    let config = RelayClientConfig {
        relay_url: format!("ws://{addr}/v1/ws"),
        pairing_ttl: 300,
        seed: SeedSource::Bytes([1u8; 32]),
    };
    let client = RelayClient::new(pool, config).await.expect("new client");

    let agent_ed = client.agent_ed25519_pub();
    let agent_x = client.agent_x25519_pub();
    let ns_raw: [u8; 32] = hex::decode(client.namespace_id_hex()).unwrap().try_into().unwrap();

    // Mobile identity.
    let mobile = crypto::derive_keys(&[7u8; 32]);
    let mobile_sk = mobile.signing_key();
    let mobile_ed = mobile.ed25519_pub;
    // Shared AES key both sides derive independently.
    let aes = crypto::derive_aes_key(&crypto::ecdh(&mobile.x25519_priv, &agent_x));

    let mut rx = client.events();
    client.start().await.expect("start");

    // Connected handshake completes.
    match next_event(&mut rx).await {
        RelayEvent::Connected => {}
        other => panic!("expected Connected, got {other:?}"),
    }

    // 1) Open a pairing window and pair the mobile. `start_pairing` only queues
    // the frame; let the relay register the token before the mobile pairs.
    let started = client.start_pairing(0).await.expect("start_pairing");
    tokio::time::sleep(Duration::from_millis(150)).await;
    pair(addr, &mobile_sk, &ns_raw, &started.token, &mobile.x25519_pub).await;

    match next_significant(&mut rx).await {
        RelayEvent::ClientPaired { ed25519_pub, platform, .. } => {
            assert_eq!(ed25519_pub, mobile_ed);
            assert_eq!(platform, "ios");
        }
        other => panic!("expected ClientPaired, got {other:?}"),
    }

    // The device is Pending until we authorize it.
    let clients = client.list_clients().await;
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].state, skald_relay_client::ClientState::Pending);

    // 2) Authorize, then the mobile connects as the authorized client role.
    client.authorize(&mobile_ed).await.expect("authorize");
    // Give the relay a moment to process the Authorize set before connecting.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut mobile_ws = auth_client(addr, &mobile_sk, &ns_raw).await;

    // 3) Agent → mobile: send an opaque payload; the mobile decrypts it.
    let agent_payload = b"hello-from-agent";
    client.send(&mobile_ed, agent_payload, false).await.expect("send");
    let frame = recv(&mut mobile_ws).await;
    let m = match frame.frame {
        Some(Frame::Message(m)) => m,
        other => panic!("mobile expected Message, got {other:?}"),
    };
    assert_eq!(m.peer.as_ref(), &agent_ed[..], "relay rewrites peer=from");
    let nonce: [u8; 12] = m.nonce.as_ref().try_into().unwrap();
    let aad = crypto::build_aad(&ns_raw, &agent_ed, &mobile_ed);
    let framed = crypto::open(&aes, &nonce, &aad, &m.ciphertext).expect("mobile open");
    let got = crypto::decompress_payload(&framed).expect("decompress");
    assert_eq!(got, agent_payload);

    // 4) Mobile → agent: seal a reply (counter 1, client→agent direction).
    let reply = b"hi-from-mobile";
    let reply_nonce = crypto::build_nonce(DIR_CLIENT_TO_AGENT, 1);
    let reply_aad = crypto::build_aad(&ns_raw, &mobile_ed, &agent_ed);
    let reply_framed = crypto::compress_payload(reply);
    let reply_ct = crypto::seal(&aes, &reply_nonce, &reply_aad, &reply_framed).expect("mobile seal");
    let reply_frame = RelayFrame {
        frame: Some(Frame::Message(ProtoMessage {
            ciphertext: Bytes::copy_from_slice(&reply_ct),
            nonce: Bytes::copy_from_slice(&reply_nonce),
            peer: Bytes::copy_from_slice(&agent_ed),
            live: false,
        })),
    };
    send(&mut mobile_ws, &reply_frame).await;

    match next_significant(&mut rx).await {
        RelayEvent::Message { from, payload, .. } => {
            assert_eq!(from, mobile_ed);
            assert_eq!(payload, reply);
        }
        other => panic!("expected Message, got {other:?}"),
    }

    // 5) Replay the exact same frame (counter 1 again) → dropped, no event.
    send(&mut mobile_ws, &reply_frame).await;
    let replayed = tokio::time::timeout(Duration::from_millis(400), rx.recv()).await;
    assert!(
        !matches!(replayed, Ok(Ok(RelayEvent::Message { .. }))),
        "a replayed counter must not surface a Message event"
    );

    // 6) Revoke the device → ClientRevoked event + empty registry.
    client.revoke(&mobile_ed).await.expect("revoke");
    match next_significant(&mut rx).await {
        RelayEvent::ClientRevoked { ed25519_pub } => assert_eq!(ed25519_pub, mobile_ed),
        other => panic!("expected ClientRevoked, got {other:?}"),
    }
    assert!(client.list_clients().await.is_empty(), "registry empty after revoke");

    client.shutdown().await;
}

/// `clear_all` removes every device and emits one `ClientRevoked` per device.
#[tokio::test]
async fn clear_all_wipes_devices() {
    let addr = spawn_relay().await;
    let pool = client_pool().await;
    let client = RelayClient::new(
        pool,
        RelayClientConfig {
            relay_url: format!("ws://{addr}/v1/ws"),
            pairing_ttl: 300,
            seed: SeedSource::Bytes([2u8; 32]),
        },
    )
    .await
    .expect("new client");

    let ns_raw: [u8; 32] = hex::decode(client.namespace_id_hex()).unwrap().try_into().unwrap();
    let mut rx = client.events();
    client.start().await.expect("start");
    match next_event(&mut rx).await {
        RelayEvent::Connected => {}
        other => panic!("expected Connected, got {other:?}"),
    }

    // Pair + authorize one device.
    let mobile = crypto::derive_keys(&[9u8; 32]);
    let started = client.start_pairing(0).await.expect("pairing");
    tokio::time::sleep(Duration::from_millis(150)).await;
    pair(addr, &mobile.signing_key(), &ns_raw, &started.token, &mobile.x25519_pub).await;
    match next_significant(&mut rx).await {
        RelayEvent::ClientPaired { .. } => {}
        other => panic!("expected ClientPaired, got {other:?}"),
    }
    client.authorize(&mobile.ed25519_pub).await.expect("authorize");
    assert_eq!(client.list_clients().await.len(), 1);

    client.clear_all().await.expect("clear_all");
    match next_significant(&mut rx).await {
        RelayEvent::ClientRevoked { ed25519_pub } => assert_eq!(ed25519_pub, mobile.ed25519_pub),
        other => panic!("expected ClientRevoked, got {other:?}"),
    }
    assert!(client.list_clients().await.is_empty());

    client.shutdown().await;
}
