//! End-to-end protocol tests for the v2 relay transport
//! (data/iOS-app/v2/relay-protocol.md). Speaks protobuf binary frames over
//! WebSocket against a real axum server bound to an ephemeral port.
//!
//! Every post-upgrade WS frame is a binary frame (opcode `0x2`) that carries
//! exactly one `RelayFrame` protobuf message. The relay speaks first
//! (`Challenge`), then the client authenticates with an Ed25519 signature over
//! `AUTH_DOMAIN ‖ 0x00 ‖ challenge_nonce_raw(32B)`; see
//! `skald_relay_common::crypto::challenge_message`.

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use ed25519_dalek::{Signer, SigningKey};
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use sha2::{Digest, Sha256};
use skald_relay_common::proto::v2::{
    self, Auth, AuthAgent, AuthClient, AuthError, AuthOk, AuthPairing, Authorize, AuthorizeOk,
    ClientPaired, Message as ProtoMessage, PairingReady, PairingStart, PeerOffline, PresenceEvent,
    PresenceList, PresenceRequest, RelayFrame,
};
use skald_relay_common::proto::v2::auth::Role as AuthRole;
use skald_relay_common::proto::v2::relay_frame::Frame;
use tokio_tungstenite::tungstenite::Message;

use skald_relay_server::config::Config;
use skald_relay_server::{AppState, router};

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

/// Boot a relay on a random port with a throwaway SQLite file. Returns its addr.
///
/// Each test gets its own DB file. We use `std::process::id()` + a per-call
/// counter (incremented atomically across the whole process) so two tests
/// calling `spawn_relay()` in parallel — even on the same nanosecond — never
/// collide on the file path. A `spawn-relay-tests` counter is also fine, but
/// `AtomicU64` is independent of any test framework / test name.
async fn spawn_relay() -> SocketAddr {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let db = std::env::temp_dir().join(format!(
        "relay-it-{nanos}-{}-{seq}.db",
        std::process::id()
    ));
    let cfg = Config {
        bind: "127.0.0.1:0".parse().expect("bind addr"),
        db_path: db.to_string_lossy().into(),
        pipe: skald_relay_server::config::PipeConfig::default(),
    };
    let state = AppState::build(cfg).await.expect("build state");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("serve");
    });
    addr
}

async fn connect(addr: SocketAddr) -> Ws {
    let url = format!("ws://{addr}/v1/ws");
    let (ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .expect("connect");
    ws
}

/// Send a protobuf `RelayFrame` as a WebSocket **binary** frame (v2 transport,
/// relay-protocol.md §1).
async fn send(ws: &mut Ws, frame: &RelayFrame) {
    let bytes = frame.encode_to_vec();
    ws.send(Message::Binary(bytes.into()))
        .await
        .expect("send binary");
}

/// Read the next `RelayFrame`. WS-level Ping/Pong are silently consumed
/// (axum/tokio-tungstenite handle the actual pong). A `Text` frame or a WS
/// `Close` is a protocol violation under v2 — we panic with a clear message.
async fn recv(ws: &mut Ws) -> RelayFrame {
    loop {
        let m = ws.next().await.expect("stream open").expect("ws frame");
        match m {
            Message::Binary(b) => {
                return RelayFrame::decode(b.as_ref()).expect("decode protobuf");
            }
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(f) => panic!("unexpected ws close: {f:?}"),
            Message::Text(t) => panic!("unexpected text frame in v2 transport: {t}"),
            other => panic!("unexpected ws frame: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Frame builders + crypto helpers
// ---------------------------------------------------------------------------

/// Sign the v2 challenge message: `AUTH_DOMAIN ‖ 0x00 ‖ nonce(32B)`.
/// Mirrors `skald_relay_common::crypto::challenge_message` exactly.
fn sign_challenge(sk: &SigningKey, nonce: &[u8; 32]) -> [u8; 64] {
    let mut msg = Vec::with_capacity(b"skald-relay-auth-v1".len() + 1 + 32);
    msg.extend_from_slice(b"skald-relay-auth-v1");
    msg.push(0);
    msg.extend_from_slice(nonce);
    sk.sign(&msg).to_bytes()
}

/// `namespace_id` = `hex(SHA256(NS_DOMAIN ‖ 0x00 ‖ agent_ed25519_pub))`
/// (crypto.md §7). Returns the raw 32-byte value and the lowercase hex string.
fn namespace_id(pubkey: &[u8; 32]) -> ([u8; 32], String) {
    let mut h = Sha256::new();
    h.update(b"skald-namespace-v1");
    h.update([0u8]);
    h.update(pubkey);
    let raw = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&raw);
    (out, hex::encode(raw))
}

/// Read the relay's first frame — must be `RelayFrame::Challenge{nonce}`.
async fn read_challenge(ws: &mut Ws) -> [u8; 32] {
    let frame = recv(ws).await;
    match frame.frame {
        Some(Frame::Challenge(c)) => c.nonce.as_ref().try_into().expect("32B challenge"),
        other => panic!("expected Challenge, got {other:?}"),
    }
}

/// `Auth{role=Agent(...), signature}` — agent handshake.
fn auth_agent_frame(sk: &SigningKey, challenge: &[u8; 32]) -> RelayFrame {
    let sig = sign_challenge(sk, challenge);
    let pubkey = sk.verifying_key().to_bytes();
    RelayFrame {
        frame: Some(Frame::Auth(Auth {
            signature: Bytes::copy_from_slice(&sig),
            role: Some(AuthRole::Agent(AuthAgent {
                agent_ed25519_pub: Bytes::copy_from_slice(&pubkey),
            })),
        })),
    }
}

/// `Auth{role=Client(...), signature}` — client handshake.
fn auth_client_frame(sk: &SigningKey, challenge: &[u8; 32], ns_hex: &str) -> RelayFrame {
    let sig = sign_challenge(sk, challenge);
    let pubkey = sk.verifying_key().to_bytes();
    let ns_raw: [u8; 32] = hex::decode(ns_hex).expect("ns hex").try_into().expect("32B ns");
    RelayFrame {
        frame: Some(Frame::Auth(Auth {
            signature: Bytes::copy_from_slice(&sig),
            role: Some(AuthRole::Client(AuthClient {
                namespace_id: Bytes::copy_from_slice(&ns_raw),
                client_ed25519_pub: Bytes::copy_from_slice(&pubkey),
                device_token: "devtok".into(),
                platform: v2::Platform::Ios as i32,
            })),
        })),
    }
}

/// `Auth{role=Pairing(...), signature}` — short-lived pairing connection.
#[allow(clippy::too_many_arguments)]
fn auth_pairing_frame(
    sk: &SigningKey,
    challenge: &[u8; 32],
    ns_hex: &str,
    token: &[u8; 32],
    x25519_pub: &[u8; 32],
) -> RelayFrame {
    let sig = sign_challenge(sk, challenge);
    let pubkey = sk.verifying_key().to_bytes();
    let ns_raw: [u8; 32] = hex::decode(ns_hex).expect("ns hex").try_into().expect("32B ns");
    RelayFrame {
        frame: Some(Frame::Auth(Auth {
            signature: Bytes::copy_from_slice(&sig),
            role: Some(AuthRole::Pairing(AuthPairing {
                namespace_id: Bytes::copy_from_slice(&ns_raw),
                client_ed25519_pub: Bytes::copy_from_slice(&pubkey),
                client_x25519_pub: Bytes::copy_from_slice(x25519_pub),
                pairing_token: Bytes::copy_from_slice(token),
                device_token: "devtok".into(),
                platform: v2::Platform::Ios as i32,
            })),
        })),
    }
}

/// Authenticate as `agent`; returns the live connection and the namespace hex.
async fn auth_agent(addr: SocketAddr, sk: &SigningKey) -> (Ws, String) {
    let pubkey = sk.verifying_key().to_bytes();
    let mut ws = connect(addr).await;
    let challenge = read_challenge(&mut ws).await;
    send(&mut ws, &auth_agent_frame(sk, &challenge)).await;
    let frame = recv(&mut ws).await;
    let AuthOk { namespace_id: ns_bytes } = match frame.frame {
        Some(Frame::AuthOk(ok)) => ok,
        other => panic!("expected AuthOk, got {other:?}"),
    };
    let ns_hex = hex::encode(&ns_bytes);
    let (want_raw, want_hex) = namespace_id(&pubkey);
    assert_eq!(
        ns_hex, want_hex,
        "AuthOk.namespace_id must match SHA256(NS_DOMAIN‖0x00‖pubkey)"
    );
    // The wire carries the raw 32B value; compare bytes too.
    assert_eq!(ns_bytes.as_ref(), want_raw.as_ref());
    (ws, ns_hex)
}

/// Authenticate as `client`; returns the live connection. Caller is
/// responsible for draining the agent-side `PresenceEvent{ONLINE}` that the
/// relay broadcasts on auth_ok.
async fn auth_client(addr: SocketAddr, sk: &SigningKey, ns_hex: &str) -> Ws {
    let mut ws = connect(addr).await;
    let challenge = read_challenge(&mut ws).await;
    send(&mut ws, &auth_client_frame(sk, &challenge, ns_hex)).await;
    let frame = recv(&mut ws).await;
    match frame.frame {
        Some(Frame::AuthOk(_)) => {}
        other => panic!("expected AuthOk, got {other:?}"),
    }
    ws
}

/// `PairingStart{pairing_token, ttl}` — open a pairing window on the agent.
async fn send_pairing_start(ws: &mut Ws, token: &[u8; 32], ttl: u32) {
    let frame = RelayFrame {
        frame: Some(Frame::PairingStart(PairingStart {
            pairing_token: Bytes::copy_from_slice(token),
            ttl,
        })),
    };
    send(ws, &frame).await;
}

/// `Authorize{clients[]}` — replace-semantics on the authorized set.
async fn send_authorize(ws: &mut Ws, clients: &[[u8; 32]]) {
    let frame = RelayFrame {
        frame: Some(Frame::Authorize(Authorize {
            clients: clients
                .iter()
                .map(|c| Bytes::copy_from_slice(c))
                .collect(),
        })),
    };
    send(ws, &frame).await;
}

/// End-to-end pairing flow on a side connection: `challenge → auth(pairing)
/// → AuthOk → close`. Returns the freshly-paired `client_pub` and the
/// `x25519_pub` we lied about — the relay never inspects X25519 material.
async fn pair_client(
    addr: SocketAddr,
    client_sk: &SigningKey,
    ns_hex: &str,
    token: &[u8; 32],
    x25519_pub: [u8; 32],
) -> [u8; 32] {
    let mut pairing = connect(addr).await;
    let c = read_challenge(&mut pairing).await;
    send(
        &mut pairing,
        &auth_pairing_frame(client_sk, &c, ns_hex, token, &x25519_pub),
    )
    .await;
    let ok = recv(&mut pairing).await;
    match ok.frame {
        Some(Frame::AuthOk(_)) => {}
        other => panic!("pairing expected AuthOk, got {other:?}"),
    };
    // The relay sends a Close after AuthOk on a pairing connection — draining
    // the next frame is optional; let it drop here.
    drop(pairing);
    client_sk.verifying_key().to_bytes()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Agent-side happy path: the relay speaks first, returns `AuthOk` with the
/// correct 32-byte `namespace_id`, and accepts no further peer (just registers
/// the agent in the registry). Ported from the v1 test, but speaks binary
/// protobuf.
#[tokio::test]
async fn agent_handshake_creates_namespace() {
    let addr = spawn_relay().await;
    let sk = SigningKey::from_bytes(&[1u8; 32]);
    let (_agent, ns) = auth_agent(addr, &sk).await;
    let (want_raw, want_hex) = namespace_id(&sk.verifying_key().to_bytes());
    assert_eq!(hex::encode(want_raw), ns);
    assert_eq!(ns, want_hex);
}

/// A signature that doesn't cover the real challenge must be rejected with
/// `AuthError{code = "invalid_signature"}`. The relay closes the socket right
/// after; we drain the Close so the test doesn't panic on it.
#[tokio::test]
async fn bad_signature_is_rejected() {
    let addr = spawn_relay().await;
    let sk = SigningKey::from_bytes(&[1u8; 32]);
    let pubkey = sk.verifying_key().to_bytes();
    let mut ws = connect(addr).await;
    let _challenge = read_challenge(&mut ws).await;
    // Sign a different message — the signature won't verify against the
    // real challenge nonce.
    let bogus = sk.sign(b"not the challenge").to_bytes();
    send(
        &mut ws,
        &RelayFrame {
            frame: Some(Frame::Auth(Auth {
                signature: Bytes::copy_from_slice(&bogus),
                role: Some(AuthRole::Agent(AuthAgent {
                    agent_ed25519_pub: Bytes::copy_from_slice(&pubkey),
                })),
            })),
        },
    )
    .await;
    let err = recv(&mut ws).await;
    let AuthError { code, message: _ } = match err.frame {
        Some(Frame::AuthError(e)) => e,
        other => panic!("expected AuthError, got {other:?}"),
    };
    assert_eq!(code, "invalid_signature");
    // The relay follows AuthError with a Close — drain it so we exit cleanly.
    let close = ws.next().await.expect("stream").expect("ws frame");
    assert!(matches!(close, Message::Close(_)));
}

/// A client that never paired/was authorized cannot connect as `client`. The
/// relay must answer `AuthError{code = "unauthorized"}` and close.
#[tokio::test]
async fn unauthorized_client_is_rejected() {
    let addr = spawn_relay().await;
    let agent_sk = SigningKey::from_bytes(&[1u8; 32]);
    let (_agent, ns) = auth_agent(addr, &agent_sk).await;

    let client_sk = SigningKey::from_bytes(&[2u8; 32]);
    let mut ws = connect(addr).await;
    let challenge = read_challenge(&mut ws).await;
    send(&mut ws, &auth_client_frame(&client_sk, &challenge, &ns)).await;
    let err = recv(&mut ws).await;
    let AuthError { code, .. } = match err.frame {
        Some(Frame::AuthError(e)) => e,
        other => panic!("expected AuthError, got {other:?}"),
    };
    assert_eq!(code, "unauthorized");
    let close = ws.next().await.expect("stream").expect("ws frame");
    assert!(matches!(close, Message::Close(_)));
}

/// End-to-end pairing → `Authorize` → E2E `Message` flow. The relay must:
/// 1. Accept a `PairingStart` from the agent and respond with `PairingReady`.
/// 2. Accept a short-lived `auth(pairing)` connection, close it, and forward
///    `ClientPaired` to the agent.
/// 3. Accept an `Authorize` and reply with `AuthorizeOk{authorized: 1}`.
/// 4. Accept the `auth(client)` connection, send `AuthOk`, and broadcast
///    `PresenceEvent{ONLINE}` to the agent.
/// 5. Forward `Message{live:false}` agent→client, rewriting `peer = from` and
///    passing `ciphertext`/`nonce` byte-for-byte; same for client→agent.
#[tokio::test]
async fn pairing_authorize_and_live_message() {
    let addr = spawn_relay().await;
    let agent_sk = SigningKey::from_bytes(&[1u8; 32]);
    let agent_pub = agent_sk.verifying_key().to_bytes();
    let (mut agent, ns) = auth_agent(addr, &agent_sk).await;

    // 1) Agent opens a pairing window.
    let token = [0x11u8; 32];
    send_pairing_start(&mut agent, &token, 300).await;
    let ready = recv(&mut agent).await;
    let PairingReady { ttl } = match ready.frame {
        Some(Frame::PairingReady(p)) => p,
        other => panic!("expected PairingReady, got {other:?}"),
    };
    assert_eq!(ttl, 300);

    // 2) Client pairs on a side connection.
    let client_sk = SigningKey::from_bytes(&[2u8; 32]);
    let client_x = [0x33u8; 32]; // opaque X25519 pubkey; relay never inspects
    let client_pub = pair_client(addr, &client_sk, &ns, &token, client_x).await;
    assert_eq!(client_pub, client_sk.verifying_key().to_bytes());

    // 3) Agent is told a device paired.
    let paired = recv(&mut agent).await;
    let ClientPaired {
        client_ed25519_pub,
        client_x25519_pub,
        platform,
    } = match paired.frame {
        Some(Frame::ClientPaired(p)) => p,
        other => panic!("expected ClientPaired, got {other:?}"),
    };
    assert_eq!(client_ed25519_pub.as_ref(), &client_pub[..]);
    assert_eq!(client_x25519_pub.as_ref(), &client_x[..]);
    assert_eq!(platform, v2::Platform::Ios as i32);

    // 4) Agent authorizes the client.
    send_authorize(&mut agent, &[client_pub]).await;
    let authorized = recv(&mut agent).await;
    let AuthorizeOk { authorized } = match authorized.frame {
        Some(Frame::AuthorizeOk(a)) => a,
        other => panic!("expected AuthorizeOk, got {other:?}"),
    };
    assert_eq!(authorized, 1);

    // 5) Client connects as the authorized role.
    let mut client = auth_client(addr, &client_sk, &ns).await;

    // 5a) Drain the agent-side PresenceEvent{ONLINE} for the new client.
    let pe = recv(&mut agent).await;
    let PresenceEvent { pubkey, status } = match pe.frame {
        Some(Frame::PresenceEvent(p)) => p,
        other => panic!("expected PresenceEvent, got {other:?}"),
    };
    assert_eq!(pubkey.as_ref(), &client_pub[..]);
    assert_eq!(status, v2::Status::Online as i32);

    // 6) Agent → client Message{live:false}. The relay stamps `peer = from`
    // (the agent's pubkey) and forwards `ciphertext`/`nonce` byte-for-byte.
    let nonce = [0u8; 12];
    let ciphertext = b"hello world";
    send(
        &mut agent,
        &RelayFrame {
            frame: Some(Frame::Message(ProtoMessage {
                ciphertext: Bytes::copy_from_slice(ciphertext),
                nonce: Bytes::copy_from_slice(&nonce),
                peer: Bytes::copy_from_slice(&client_pub),
                live: false,
            })),
        },
    )
    .await;
    let msg = recv(&mut client).await;
    let ProtoMessage {
        ciphertext: ct,
        nonce: n,
        peer: from,
        live,
    } = match msg.frame {
        Some(Frame::Message(m)) => m,
        other => panic!("expected Message, got {other:?}"),
    };
    assert_eq!(ct.as_ref(), ciphertext);
    assert_eq!(n.as_ref(), &nonce[..]);
    assert_eq!(from.as_ref(), &agent_pub[..]);
    assert!(!live, "relay must rewrite live=false on delivery");

    // 7) Client → agent reply routes back.
    let reply_ct = b"reply";
    let reply_nonce: [u8; 12] = [0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1];
    send(
        &mut client,
        &RelayFrame {
            frame: Some(Frame::Message(ProtoMessage {
                ciphertext: Bytes::copy_from_slice(reply_ct),
                nonce: Bytes::copy_from_slice(&reply_nonce),
                peer: Bytes::copy_from_slice(&agent_pub),
                live: false,
            })),
        },
    )
    .await;
    let back = recv(&mut agent).await;
    let ProtoMessage {
        ciphertext: ct,
        nonce: n,
        peer: from,
        ..
    } = match back.frame {
        Some(Frame::Message(m)) => m,
        other => panic!("expected Message back, got {other:?}"),
    };
    assert_eq!(ct.as_ref(), reply_ct);
    assert_eq!(n.as_ref(), &reply_nonce[..]);
    assert_eq!(from.as_ref(), &client_pub[..]);
}

/// v2 live channel: `Message{live:true}` is route-or-fail. If the destination
/// isn't connected, the relay answers the sender with `PeerOffline{peer}` —
/// no enqueue, no push.
///
/// To exercise this we register an authorized client that never connects as
/// `client` (so its `client_tx` is None), then have the agent live-send to
/// it. The relay must return `PeerOffline`.
#[tokio::test]
async fn live_message_to_offline_peer_returns_peer_offline() {
    let addr = spawn_relay().await;
    let agent_sk = SigningKey::from_bytes(&[1u8; 32]);
    let (mut agent, ns) = auth_agent(addr, &agent_sk).await;

    let client_sk = SigningKey::from_bytes(&[2u8; 32]);
    let client_pub = client_sk.verifying_key().to_bytes();
    // Pair + authorize (mirrors the full flow) — the client never connects.
    let token = [0x11u8; 32];
    send_pairing_start(&mut agent, &token, 300).await;
    let _ = recv(&mut agent).await; // PairingReady
    let _paired = pair_client(addr, &client_sk, &ns, &token, [0x33; 32]).await;
    let cp = recv(&mut agent).await; // ClientPaired
    assert!(matches!(cp.frame, Some(Frame::ClientPaired(_))));
    send_authorize(&mut agent, &[client_pub]).await;
    let _ = recv(&mut agent).await; // AuthorizeOk

    // The client is registered as authorized but never connects as `client`,
    // so its `client_tx` is None. The relay must return PeerOffline.
    let nonce = [0u8; 12];
    let ct = vec![0u8; 32];
    send(
        &mut agent,
        &RelayFrame {
            frame: Some(Frame::Message(ProtoMessage {
                ciphertext: Bytes::copy_from_slice(&ct),
                nonce: Bytes::copy_from_slice(&nonce),
                peer: Bytes::copy_from_slice(&client_pub),
                live: true,
            })),
        },
    )
    .await;
    let resp = recv(&mut agent).await;
    let PeerOffline { peer } = match resp.frame {
        Some(Frame::PeerOffline(p)) => p,
        other => panic!("expected PeerOffline, got {other:?}"),
    };
    assert_eq!(peer.as_ref(), &client_pub[..]);
}

/// `PresenceRequest` → `PresenceList{online[]}` snapshot, scoped to the
/// requester's namespace, includes every connected peer (agent + clients).
#[tokio::test]
async fn presence_list_returns_online_peers() {
    let addr = spawn_relay().await;
    let agent_sk = SigningKey::from_bytes(&[1u8; 32]);
    let agent_pub = agent_sk.verifying_key().to_bytes();
    let (mut agent, ns) = auth_agent(addr, &agent_sk).await;

    // Pair + authorize a client, then connect it.
    let client_sk = SigningKey::from_bytes(&[2u8; 32]);
    let client_pub = client_sk.verifying_key().to_bytes();
    let token = [0x11u8; 32];
    send_pairing_start(&mut agent, &token, 300).await;
    let _ = recv(&mut agent).await; // PairingReady
    let _paired = pair_client(addr, &client_sk, &ns, &token, [0x33; 32]).await;
    let _ = recv(&mut agent).await; // ClientPaired
    send_authorize(&mut agent, &[client_pub]).await;
    let _ = recv(&mut agent).await; // AuthorizeOk

    let _client = auth_client(addr, &client_sk, &ns).await;

    // Drain the ONLINE presence event from the agent.
    let pe = recv(&mut agent).await;
    let PresenceEvent { pubkey, status } = match pe.frame {
        Some(Frame::PresenceEvent(p)) => p,
        other => panic!("expected PresenceEvent, got {other:?}"),
    };
    assert_eq!(pubkey.as_ref(), &client_pub[..]);
    assert_eq!(status, v2::Status::Online as i32);

    // Now ask for the namespace's presence snapshot.
    send(
        &mut agent,
        &RelayFrame {
            frame: Some(Frame::PresenceRequest(PresenceRequest {})),
        },
    )
    .await;
    let list = recv(&mut agent).await;
    let PresenceList { online } = match list.frame {
        Some(Frame::PresenceList(p)) => p,
        other => panic!("expected PresenceList, got {other:?}"),
    };
    let mut got: Vec<[u8; 32]> = online
        .iter()
        .map(|b| b.as_ref().try_into().expect("32B pubkey"))
        .collect();
    got.sort();
    let mut want = vec![agent_pub, client_pub];
    want.sort();
    assert_eq!(got, want, "PresenceList must contain agent + client pubkeys");
}

/// `PresenceEvent{ONLINE}` is broadcast at the peer's `auth_ok`. When the
/// client disconnects, `PresenceEvent{OFFLINE}` is broadcast to the other
/// members of the namespace.
#[tokio::test]
async fn presence_event_on_auth_ok_and_disconnect() {
    let addr = spawn_relay().await;
    let agent_sk = SigningKey::from_bytes(&[1u8; 32]);
    let (mut _agent, ns) = auth_agent(addr, &agent_sk).await;

    let client_sk = SigningKey::from_bytes(&[2u8; 32]);
    let client_pub = client_sk.verifying_key().to_bytes();
    let token = [0x11u8; 32];
    send_pairing_start(&mut _agent, &token, 300).await;
    let _ = recv(&mut _agent).await; // PairingReady
    let _paired = pair_client(addr, &client_sk, &ns, &token, [0x33; 32]).await;
    let _ = recv(&mut _agent).await; // ClientPaired
    send_authorize(&mut _agent, &[client_pub]).await;
    let _ = recv(&mut _agent).await; // AuthorizeOk

    // Connect the client. The agent must see PresenceEvent{ONLINE} for it.
    let mut client = auth_client(addr, &client_sk, &ns).await;
    let pe_on = recv(&mut _agent).await;
    let PresenceEvent { pubkey, status } = match pe_on.frame {
        Some(Frame::PresenceEvent(p)) => p,
        other => panic!("expected PresenceEvent (online), got {other:?}"),
    };
    assert_eq!(pubkey.as_ref(), &client_pub[..]);
    assert_eq!(status, v2::Status::Online as i32);

    // Drop the client. The relay must broadcast PresenceEvent{OFFLINE} to the
    // agent. Dropping a tungstenite stream sends a WS Close; the agent's
    // reader task observes the end-of-stream and runs the disconnect
    // cleanup.
    drop(client);
    // Give the agent's reader task time to detect the close and broadcast
    // OFFLINE. 100ms is plenty on a fast loopback; bump if flaky on CI.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let pe_off = recv(&mut _agent).await;
    let PresenceEvent { pubkey, status } = match pe_off.frame {
        Some(Frame::PresenceEvent(p)) => p,
        other => panic!("expected PresenceEvent (offline), got {other:?}"),
    };
    assert_eq!(pubkey.as_ref(), &client_pub[..]);
    assert_eq!(status, v2::Status::Offline as i32);
}

/// `Message{live:false}` to an offline peer: the relay enqueues the message
/// and never returns `PeerOffline`. (The `live=true` counterpart is covered
/// by `live_message_to_offline_peer_returns_peer_offline`.) We assert the
/// negative invariant: no `PeerOffline` arrives at the sender.
#[tokio::test]
async fn store_and_forward_when_peer_offline() {
    let addr = spawn_relay().await;
    let agent_sk = SigningKey::from_bytes(&[1u8; 32]);
    let (mut agent, _ns) = auth_agent(addr, &agent_sk).await;

    let client_sk = SigningKey::from_bytes(&[2u8; 32]);
    let client_pub = client_sk.verifying_key().to_bytes();
    let token = [0x11u8; 32];
    send_pairing_start(&mut agent, &token, 300).await;
    let _ = recv(&mut agent).await; // PairingReady
    let _paired = pair_client(addr, &client_sk, &_ns, &token, [0x33; 32]).await;
    let _ = recv(&mut agent).await; // ClientPaired
    send_authorize(&mut agent, &[client_pub]).await;
    let _ = recv(&mut agent).await; // AuthorizeOk

    // Send `live=false` to the offline client. We give the relay a moment
    // to enqueue and (best-effort) push, then assert that the next frame
    // the agent reads is NOT a PeerOffline.
    let nonce = [0u8; 12];
    let ct = vec![0u8; 32];
    send(
        &mut agent,
        &RelayFrame {
            frame: Some(Frame::Message(ProtoMessage {
                ciphertext: Bytes::copy_from_slice(&ct),
                nonce: Bytes::copy_from_slice(&nonce),
                peer: Bytes::copy_from_slice(&client_pub),
                live: false,
            })),
        },
    )
    .await;
    // No frame should be coming back. Race against a short timeout.
    let r = tokio::time::timeout(std::time::Duration::from_millis(150), recv(&mut agent)).await;
    match r {
        Err(_) => { /* expected: no response on live=false */ }
        Ok(RelayFrame {
            frame: Some(Frame::PeerOffline(_)),
        }) => panic!("live=false must NOT trigger PeerOffline"),
        Ok(other) => panic!("unexpected frame on live=false: {other:?}"),
    }
}
