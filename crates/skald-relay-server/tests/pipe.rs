//! End-to-end tests for the `/v1/pipe` data plane (docs/relay/pipe.md §2).
//!
//! Two raw WebSocket peers authenticate to the relay (MsgPack `pipe_auth`,
//! Ed25519 signature), get matched by `connection_id`, and stream opaque bytes
//! the relay never reads. Covers the happy path plus the auth/cross-dest
//! rejections.

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt};
use skald_relay_common::crypto;
use skald_relay_common::pipe::{self, PipeAuth, PipeChallenge};
use tokio_tungstenite::tungstenite::Message;

use skald_relay_server::config::{Config, PipeConfig};
use skald_relay_server::{AppState, router};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Boot a relay with a throwaway DB, returning its addr and the shared state so
/// tests can seed the namespace / authorized clients directly.
async fn spawn_relay() -> (SocketAddr, AppState) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let db = std::env::temp_dir().join(format!("relay-pipe-it-{nanos}-{}-{seq}.db", std::process::id()));
    let cfg = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        db_path: db.to_string_lossy().into(),
        pipe: PipeConfig::default(),
    };
    let state = AppState::build(cfg).await.expect("build state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_state = state.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router(serve_state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    (addr, state)
}

/// An identity = its Ed25519 signing key + derived pubkeys.
struct Id {
    sk: SigningKey,
    ed_pub: [u8; 32],
}

fn id_from_seed(seed: u8) -> Id {
    let dk = crypto::derive_keys(&[seed; 32]);
    Id { sk: SigningKey::from_bytes(&dk.ed25519_priv), ed_pub: dk.ed25519_pub }
}

/// Seed a namespace owned by `agent` and authorize `client` in it.
async fn seed_namespace(state: &AppState, agent: &Id, client: &Id) -> [u8; 32] {
    let (ns_raw, ns_hex) = crypto::namespace_id(&agent.ed_pub);
    state.store.upsert_namespace(&ns_hex, &agent.ed_pub).await.unwrap();
    let client_x = crypto::derive_keys(&[0xC1; 32]).x25519_pub; // any 32B is fine for membership
    state
        .store
        .upsert_pending_client(&ns_hex, &client.ed_pub, &client_x, "", "ios")
        .await
        .unwrap();
    state.store.apply_authorize(&ns_hex, &[client.ed_pub]).await.unwrap();
    ns_raw
}

/// Connect to `/v1/pipe`, complete the challenge→auth handshake for `me`
/// targeting `peer_ed`, and return the live socket. `dest_override` lets a test
/// declare the wrong counterparty (cross-dest rejection).
async fn dial_and_auth(
    addr: SocketAddr,
    me: &Id,
    peer_ed: &[u8; 32],
    ns_raw: &[u8; 32],
    connection_id: &[u8; 32],
    corrupt_sig: bool,
    dest_override: Option<[u8; 32]>,
) -> Ws {
    let url = format!("ws://{addr}/v1/pipe");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.expect("connect");

    // Relay speaks first: PipeChallenge.
    let nonce = loop {
        match ws.next().await.expect("frame").expect("ws ok") {
            Message::Binary(data) => {
                let c: PipeChallenge = pipe::decode(&data).expect("challenge");
                break pipe::to_array::<32>(&c.nonce).expect("32B nonce");
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            other => panic!("expected challenge, got {other:?}"),
        }
    };

    let mut sig = crypto::sign_pipe_auth(&me.sk, &nonce, connection_id);
    if corrupt_sig {
        sig[0] ^= 0x01;
    }
    let dest = dest_override.unwrap_or_else(|| crypto::sha256(peer_ed));
    let auth = PipeAuth {
        connection_id: connection_id.to_vec(),
        pubkey: me.ed_pub.to_vec(),
        dest: dest.to_vec(),
        namespace_id: ns_raw.to_vec(),
        signature: sig.to_vec(),
    };
    ws.send(Message::Binary(pipe::encode(&auth).into())).await.expect("send auth");
    ws
}

/// Read the next binary frame, or `None` if the socket closed/ended.
async fn next_binary(ws: &mut Ws) -> Option<Vec<u8>> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Binary(d))) => return Some(d.to_vec()),
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
            Some(Ok(Message::Close(_))) | None => return None,
            Some(Ok(_)) => continue,
            Some(Err(_)) => return None,
        }
    }
}

#[tokio::test]
async fn pipe_matches_and_splices_bytes_both_ways() {
    let (addr, state) = spawn_relay().await;
    let agent = id_from_seed(1);
    let client = id_from_seed(2);
    let ns_raw = seed_namespace(&state, &agent, &client).await;
    let cid = [0x7Au8; 32];

    // Agent dials first (becomes pending), client second (matches).
    let mut a = dial_and_auth(addr, &agent, &client.ed_pub, &ns_raw, &cid, false, None).await;
    // Small delay so A is registered pending before B arrives.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut b = dial_and_auth(addr, &client, &agent.ed_pub, &ns_raw, &cid, false, None).await;

    // A → B
    a.send(Message::Binary(b"hello-from-a".to_vec().into())).await.unwrap();
    assert_eq!(next_binary(&mut b).await.as_deref(), Some(&b"hello-from-a"[..]));
    // B → A
    b.send(Message::Binary(b"hello-from-b".to_vec().into())).await.unwrap();
    assert_eq!(next_binary(&mut a).await.as_deref(), Some(&b"hello-from-b"[..]));

    // Closing one tears down the other (no orphans).
    a.close(None).await.unwrap();
    assert_eq!(next_binary(&mut b).await, None);
}

#[tokio::test]
async fn pipe_rejects_bad_signature() {
    let (addr, state) = spawn_relay().await;
    let agent = id_from_seed(3);
    let client = id_from_seed(4);
    let ns_raw = seed_namespace(&state, &agent, &client).await;
    let cid = [0x01u8; 32];

    // Corrupt signature → relay closes without registering a pending pipe.
    let mut a = dial_and_auth(addr, &agent, &client.ed_pub, &ns_raw, &cid, true, None).await;
    assert_eq!(next_binary(&mut a).await, None, "relay must close on bad signature");
}

#[tokio::test]
async fn pipe_rejects_cross_dest_mismatch() {
    let (addr, state) = spawn_relay().await;
    let agent = id_from_seed(5);
    let client = id_from_seed(6);
    let ns_raw = seed_namespace(&state, &agent, &client).await;
    let cid = [0x02u8; 32];

    // A targets the client correctly; B (the client) declares the wrong dest
    // (points at a stranger, not the agent) → cross-ref fails, both torn down.
    let stranger = crypto::sha256(&[0xEE; 32]);
    let mut a = dial_and_auth(addr, &agent, &client.ed_pub, &ns_raw, &cid, false, None).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let mut b =
        dial_and_auth(addr, &client, &agent.ed_pub, &ns_raw, &cid, false, Some(stranger)).await;

    assert_eq!(next_binary(&mut b).await, None, "mismatched second side is closed");
    assert_eq!(next_binary(&mut a).await, None, "first side is torn down too");
}

#[tokio::test]
async fn pipe_rejects_non_member() {
    let (addr, state) = spawn_relay().await;
    let agent = id_from_seed(7);
    let client = id_from_seed(8);
    let _ns_raw = seed_namespace(&state, &agent, &client).await;
    let (ns_raw, _) = crypto::namespace_id(&agent.ed_pub);
    let outsider = id_from_seed(9); // never authorized in this namespace
    let cid = [0x03u8; 32];

    let mut a = dial_and_auth(addr, &outsider, &agent.ed_pub, &ns_raw, &cid, false, None).await;
    assert_eq!(next_binary(&mut a).await, None, "non-member must be rejected");
}
