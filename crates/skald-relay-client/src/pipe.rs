//! Pipe data-plane client (docs/relay/pipe.md §2-3): the [`PipeConnection`]
//! secure byte channel over a `/v1/pipe` WebSocket.
//!
//! The control plane (invite/accept signaling, ephemeral DH) lives in
//! [`crate::state`]; by the time `PipeConnection::connect` runs, both peers have
//! derived the same per-pipe `pipe_key`. This module only does the data plane:
//! dial `/v1/pipe`, prove identity to the relay (`pipe_auth`), then seal/open
//! every frame with AES-256-GCM keyed by `pipe_key`, using a per-direction
//! counter nonce (the relay forwards opaque ciphertext, pipe.md §2.2).

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use skald_relay_common::crypto;
use skald_relay_common::pipe::{self, PipeAuth, PipeChallenge, PipeSuite};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

/// An inbound pipe invite surfaced to the application (responder side). The app
/// inspects `from` / `stream_type` / `headers` and then calls
/// `RelayClient::accept_pipe` or `reject_pipe`. The remaining fields carry the
/// handshake state the accept path needs.
#[derive(Debug, Clone)]
pub struct IncomingPipe {
    /// The initiator's ed25519 pubkey.
    pub from: [u8; 32],
    /// App-defined purpose discriminator.
    pub stream_type: String,
    /// Arbitrary app-defined headers from the invite.
    pub headers: BTreeMap<String, String>,
    /// Rendezvous key (echoed in the accept + data-plane auth).
    pub(crate) connection_id: [u8; 32],
    /// Negotiated suite (v1: only `X25519Sealed`).
    pub(crate) suite: PipeSuite,
    /// The initiator's opaque handshake material (its ephemeral X25519 pubkey).
    pub(crate) peer_handshake: Vec<u8>,
}

type ClientWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type ClientSink = SplitSink<ClientWs, WsMessage>;
type ClientStream = SplitStream<ClientWs>;

/// Soft cap on **unflushed** outbound bytes buffered inside the pipe before
/// [`PipeSender::send`] blocks (backpressure). The background writer task
/// releases each reservation once the corresponding frame is flushed to the
/// socket, so this bounds in-flight memory while letting `send` and `recv`
/// proceed independently. ~10 MiB.
const SEND_BUFFER_BYTES: usize = 10 * 1024 * 1024;

/// Which end of the pipe this peer is — selects the send/receive nonce
/// directions so the two AES-GCM streams never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeRole {
    /// Sent `pipe_invite`. Sends on the INITIATOR direction.
    Initiator,
    /// Replied with `pipe_accept`. Sends on the RESPONDER direction.
    Responder,
}

/// Write half of a [`PipeConnection`]: seals plaintext and queues it for the
/// background writer task that owns the socket sink. Single-writer (`&mut self`)
/// so the per-direction counter nonce stays strictly ordered on the wire.
pub struct PipeSender {
    /// Sealed frames + their byte reservation, drained by the writer task in
    /// FIFO order (preserving counter order). Count-unbounded but byte-bounded
    /// by `buffer`.
    data_tx: mpsc::UnboundedSender<(Vec<u8>, OwnedSemaphorePermit)>,
    /// ~[`SEND_BUFFER_BYTES`] of permits; `send` blocks when exhausted.
    buffer: Arc<Semaphore>,
    key: [u8; 32],
    send_dir: [u8; 4],
    send_ctr: u64,
    /// AAD binding every frame to the rendezvous (the 32-byte connection_id).
    aad: [u8; 32],
}

/// Read half of a [`PipeConnection`]: reads sealed frames off the socket stream
/// and opens them. WS-level pings are answered via the shared writer task.
pub struct PipeReceiver {
    stream: ClientStream,
    /// Forwards `Pong` replies to the writer task (which owns the sink).
    ctrl_tx: mpsc::UnboundedSender<WsMessage>,
    key: [u8; 32],
    recv_dir: [u8; 4],
    recv_ctr: u64,
    aad: [u8; 32],
}

/// An end-to-end-encrypted byte channel to a namespace peer, relayed through
/// `/v1/pipe`. The relay never sees plaintext. Full-duplex: a background writer
/// task owns the socket sink and drains a byte-bounded buffer, so `send` and
/// `recv` never block each other at the socket. Use the unified `send`/`recv`
/// on `&mut self`, or [`split`](Self::split) into independent halves to drive
/// the two directions from separate tasks.
pub struct PipeConnection {
    sender: PipeSender,
    receiver: PipeReceiver,
    /// Cancels the writer task on explicit [`close`](Self::close).
    cancel: CancellationToken,
    writer: JoinHandle<()>,
}

impl PipeConnection {
    /// Dial `/v1/pipe`, complete the relay auth handshake, and return the ready
    /// channel. `pipe_key` must already be derived from the signaling ephemeral
    /// DH (same value on both peers).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn connect(
        relay_url: &str,
        signing_key: &ed25519_dalek::SigningKey,
        my_ed_pub: &[u8; 32],
        peer_ed_pub: &[u8; 32],
        namespace_id_raw: &[u8; 32],
        connection_id: &[u8; 32],
        pipe_key: &[u8; 32],
        role: PipeRole,
    ) -> Result<PipeConnection> {
        let url = pipe_url(relay_url);
        let (mut ws, _) = tokio_tungstenite::connect_async(&url).await?;

        // Relay speaks first: PipeChallenge.
        let nonce = read_challenge(&mut ws).await?;

        // Reply with a signature over PIPE_AUTH_DOMAIN ‖ 0x00 ‖ nonce ‖ cid.
        let sig = crypto::sign_pipe_auth(signing_key, &nonce, connection_id);
        let auth = PipeAuth {
            connection_id: connection_id.to_vec(),
            pubkey: my_ed_pub.to_vec(),
            dest: crypto::sha256(peer_ed_pub).to_vec(),
            namespace_id: namespace_id_raw.to_vec(),
            signature: sig.to_vec(),
        };
        ws.send(WsMessage::Binary(pipe::encode(&auth).into())).await?;

        let (send_dir, recv_dir) = match role {
            PipeRole::Initiator => (crypto::DIR_PIPE_INITIATOR, crypto::DIR_PIPE_RESPONDER),
            PipeRole::Responder => (crypto::DIR_PIPE_RESPONDER, crypto::DIR_PIPE_INITIATOR),
        };

        // Split the socket so the two directions are independent: the writer task
        // owns the sink and drains the byte-bounded buffer; the read half owns
        // the stream. Counters start at 1 (pipe.md §4).
        let (sink, stream) = ws.split();
        let buffer = Arc::new(Semaphore::new(SEND_BUFFER_BYTES));
        let (data_tx, data_rx) = mpsc::unbounded_channel::<(Vec<u8>, OwnedSemaphorePermit)>();
        let (ctrl_tx, ctrl_rx) = mpsc::unbounded_channel::<WsMessage>();
        let cancel = CancellationToken::new();
        let writer = tokio::spawn(writer_loop(sink, data_rx, ctrl_rx, cancel.clone()));

        Ok(PipeConnection {
            sender: PipeSender {
                data_tx,
                buffer,
                key: *pipe_key,
                send_dir,
                send_ctr: 1,
                aad: *connection_id,
            },
            receiver: PipeReceiver {
                stream,
                ctrl_tx,
                key: *pipe_key,
                recv_dir,
                recv_ctr: 1,
                aad: *connection_id,
            },
            cancel,
            writer,
        })
    }

    /// Seal and queue one application chunk (delegates to the write half).
    pub async fn send(&mut self, plaintext: &[u8]) -> Result<()> {
        self.sender.send(plaintext).await
    }

    /// Receive and open the next application chunk (delegates to the read half).
    /// `Ok(None)` on a clean close.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        self.receiver.recv().await
    }

    /// Split into independent write/read halves for full-duplex use: move each
    /// into its own task and the two directions run concurrently. Dropping
    /// **both** halves tears the pipe down (the writer task closes the socket
    /// once both of its channels are gone).
    pub fn split(self) -> (PipeSender, PipeReceiver) {
        // Detach the writer (drop its JoinHandle); teardown is drop-driven for
        // this path. `cancel` is dropped without firing — a dropped token does
        // not cancel — so the writer lives until both halves drop.
        (self.sender, self.receiver)
    }

    /// Cancel the writer task and close the underlying socket.
    pub async fn close(self) {
        self.cancel.cancel();
        let _ = self.writer.await;
    }
}

impl PipeSender {
    /// Seal and queue one application chunk. Returns once the frame is buffered
    /// (or, when the ~[`SEND_BUFFER_BYTES`] buffer is full, once space frees up)
    /// — **not** once it is flushed. The 12-byte nonce is implicit
    /// (per-direction counter), so it is not transmitted.
    pub async fn send(&mut self, plaintext: &[u8]) -> Result<()> {
        let nonce = crypto::build_nonce(self.send_dir, self.send_ctr);
        let sealed = crypto::seal(&self.key, &nonce, &self.aad, plaintext)
            .map_err(|e| anyhow!("pipe seal failed: {e}"))?;
        self.send_ctr += 1;
        // Reserve the frame's bytes; block here when the buffer is full. Clamp so
        // a single frame larger than the whole buffer can never deadlock.
        let want = sealed.len().min(SEND_BUFFER_BYTES) as u32;
        let permit = Arc::clone(&self.buffer)
            .acquire_many_owned(want)
            .await
            .map_err(|_| anyhow!("pipe send buffer closed"))?;
        self.data_tx
            .send((sealed, permit))
            .map_err(|_| anyhow!("pipe writer stopped"))?;
        Ok(())
    }
}

impl PipeReceiver {
    /// Receive and open the next application chunk. `Ok(None)` on a clean close.
    /// WS-level pings are answered transparently (forwarded to the writer task).
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let Some(msg) = self.stream.next().await else { return Ok(None) };
            match msg? {
                WsMessage::Binary(data) => {
                    let nonce = crypto::build_nonce(self.recv_dir, self.recv_ctr);
                    let pt = crypto::open(&self.key, &nonce, &self.aad, &data)
                        .map_err(|_| anyhow!("pipe open failed (tag mismatch / desync)"))?;
                    self.recv_ctr += 1;
                    return Ok(Some(pt));
                }
                // The writer owns the sink; hand it the Pong. A send error means
                // the pipe is tearing down anyway — ignore it.
                WsMessage::Ping(p) => {
                    let _ = self.ctrl_tx.send(WsMessage::Pong(p));
                }
                WsMessage::Pong(_) => {}
                WsMessage::Close(_) => return Ok(None),
                WsMessage::Text(_) | WsMessage::Frame(_) => {} // pipe is binary-only
            }
        }
    }
}

/// Background writer: owns the socket sink and flushes queued frames. Control
/// frames (Pong) are prioritized (`biased`) over data so keep-alives are never
/// starved by a full data buffer. Each data permit is released **after** the
/// frame is flushed, so the buffer measures unflushed bytes. Exits on `cancel`
/// or once both channels close (both halves dropped), then closes the socket.
async fn writer_loop(
    mut sink: ClientSink,
    mut data_rx: mpsc::UnboundedReceiver<(Vec<u8>, OwnedSemaphorePermit)>,
    mut ctrl_rx: mpsc::UnboundedReceiver<WsMessage>,
    cancel: CancellationToken,
) {
    let mut data_open = true;
    let mut ctrl_open = true;
    loop {
        if !data_open && !ctrl_open {
            break;
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            msg = ctrl_rx.recv(), if ctrl_open => match msg {
                Some(m) => {
                    if sink.send(m).await.is_err() {
                        break;
                    }
                }
                None => ctrl_open = false,
            },
            msg = data_rx.recv(), if data_open => match msg {
                Some((bytes, permit)) => {
                    let r = sink.send(WsMessage::Binary(bytes.into())).await;
                    drop(permit); // release the reservation after the flush
                    if r.is_err() {
                        break;
                    }
                }
                None => data_open = false,
            },
        }
    }
    let _ = sink.send(WsMessage::Close(None)).await;
    let _ = sink.close().await;
}

/// Derive the data-plane URL from the control URL by swapping the path
/// `/v1/ws` → `/v1/pipe` (config stores the full control-plane URL).
fn pipe_url(relay_url: &str) -> String {
    if relay_url.contains("/v1/ws") {
        relay_url.replace("/v1/ws", "/v1/pipe")
    } else {
        format!("{}/v1/pipe", relay_url.trim_end_matches('/'))
    }
}

/// Read frames until the relay's [`PipeChallenge`]; return the 32-byte nonce.
async fn read_challenge(ws: &mut ClientWs) -> Result<[u8; 32]> {
    while let Some(msg) = ws.next().await {
        match msg? {
            WsMessage::Binary(data) => {
                let c: PipeChallenge = pipe::decode(&data)
                    .map_err(|e| anyhow!("malformed pipe challenge: {e}"))?;
                return pipe::to_array::<32>(&c.nonce)
                    .ok_or_else(|| anyhow!("pipe challenge nonce is not 32 bytes"));
            }
            WsMessage::Ping(p) => ws.send(WsMessage::Pong(p)).await?,
            WsMessage::Close(_) => return Err(anyhow!("relay closed before pipe challenge")),
            _ => {}
        }
    }
    Err(anyhow!("connection closed before pipe challenge"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_url_swaps_path() {
        assert_eq!(pipe_url("wss://r.example/v1/ws"), "wss://r.example/v1/pipe");
        assert_eq!(pipe_url("ws://127.0.0.1:8080/v1/ws"), "ws://127.0.0.1:8080/v1/pipe");
        assert_eq!(pipe_url("wss://r.example"), "wss://r.example/v1/pipe");
    }
}

/// Data-plane E2E against the **real** relay server (booted in-process): two
/// `PipeConnection`s (initiator + responder, sharing a pre-derived key) dial
/// `/v1/pipe`, get matched, and stream sealed bytes both ways through a relay
/// that only ever sees ciphertext.
#[cfg(test)]
mod net_tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Duration;

    use skald_relay_server::config::{Config, PipeConfig};
    use skald_relay_server::{router, AppState};

    async fn spawn_relay() -> (SocketAddr, AppState) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let db = std::env::temp_dir().join(format!("relay-pipe-cli-{}-{n}.db", std::process::id()));
        let cfg = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            db_path: db.to_string_lossy().into(),
            pipe: PipeConfig::default(),
        };
        let state = AppState::build(cfg).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let serve = state.clone();
        tokio::spawn(async move {
            axum::serve(listener, router(serve).into_make_service_with_connect_info::<SocketAddr>())
                .await
                .unwrap();
        });
        (addr, state)
    }

    fn id(seed: u8) -> (ed25519_dalek::SigningKey, [u8; 32]) {
        let dk = crypto::derive_keys(&[seed; 32]);
        (dk.signing_key(), dk.ed25519_pub)
    }

    #[tokio::test]
    async fn pipe_connection_streams_through_real_relay() {
        let (addr, state) = spawn_relay().await;
        let (agent_sk, agent_ed) = id(0xA1);
        let (client_sk, client_ed) = id(0xB2);

        // Seed: agent owns the namespace, client is authorized in it.
        let (ns_raw, ns_hex) = crypto::namespace_id(&agent_ed);
        state.store.upsert_namespace(&ns_hex, &agent_ed).await.unwrap();
        let cx = crypto::derive_keys(&[0xC3; 32]).x25519_pub;
        state.store.upsert_pending_client(&ns_hex, &client_ed, &cx, "", "ios").await.unwrap();
        state.store.apply_authorize(&ns_hex, &[client_ed]).await.unwrap();

        // A pre-shared per-pipe key (in production: ephemeral DH from signaling).
        let key = crypto::derive_pipe_key(&[0x07; 32]);
        let cid = [0x9C; 32];
        let url = format!("ws://{addr}/v1/ws");

        let mut a = PipeConnection::connect(
            &url, &agent_sk, &agent_ed, &client_ed, &ns_raw, &cid, &key, PipeRole::Initiator,
        )
        .await
        .expect("initiator connect");
        tokio::time::sleep(Duration::from_millis(50)).await; // A pending before B
        let mut b = PipeConnection::connect(
            &url, &client_sk, &client_ed, &agent_ed, &ns_raw, &cid, &key, PipeRole::Responder,
        )
        .await
        .expect("responder connect");

        // Bytes both ways.
        a.send(b"ping").await.unwrap();
        assert_eq!(b.recv().await.unwrap().as_deref(), Some(&b"ping"[..]));
        b.send(b"pong").await.unwrap();
        assert_eq!(a.recv().await.unwrap().as_deref(), Some(&b"pong"[..]));

        // A larger blob round-trips intact (seal/open + relay splice).
        let blob = vec![0x5A_u8; 200_000];
        a.send(&blob).await.unwrap();
        assert_eq!(b.recv().await.unwrap(), Some(blob));

        // Closing one tears down the other.
        a.close().await;
        assert_eq!(b.recv().await.unwrap(), None);
    }

    #[tokio::test]
    async fn pipe_wrong_key_fails_to_open() {
        let (addr, state) = spawn_relay().await;
        let (agent_sk, agent_ed) = id(0xD4);
        let (client_sk, client_ed) = id(0xE5);
        let (ns_raw, ns_hex) = crypto::namespace_id(&agent_ed);
        state.store.upsert_namespace(&ns_hex, &agent_ed).await.unwrap();
        let cx = crypto::derive_keys(&[0xF6; 32]).x25519_pub;
        state.store.upsert_pending_client(&ns_hex, &client_ed, &cx, "", "ios").await.unwrap();
        state.store.apply_authorize(&ns_hex, &[client_ed]).await.unwrap();

        let cid = [0x1A; 32];
        let url = format!("ws://{addr}/v1/ws");
        // Mismatched keys: the relay still splices, but `open` must fail (the
        // relay never had the plaintext — confidentiality holds end to end).
        let ka = crypto::derive_pipe_key(&[0x01; 32]);
        let kb = crypto::derive_pipe_key(&[0x02; 32]);

        let mut a = PipeConnection::connect(
            &url, &agent_sk, &agent_ed, &client_ed, &ns_raw, &cid, &ka, PipeRole::Initiator,
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut b = PipeConnection::connect(
            &url, &client_sk, &client_ed, &agent_ed, &ns_raw, &cid, &kb, PipeRole::Responder,
        )
        .await
        .unwrap();

        a.send(b"secret").await.unwrap();
        assert!(b.recv().await.is_err(), "wrong key must fail AEAD open");
    }

    /// Full-duplex: `split` both ends, then stream a large blob **both ways at
    /// once** — each side sends while it receives. Proves send and recv run
    /// concurrently (neither direction blocks the other) through the real relay.
    #[tokio::test]
    async fn pipe_split_streams_both_directions_concurrently() {
        let (addr, state) = spawn_relay().await;
        let (agent_sk, agent_ed) = id(0x11);
        let (client_sk, client_ed) = id(0x22);
        let (ns_raw, ns_hex) = crypto::namespace_id(&agent_ed);
        state.store.upsert_namespace(&ns_hex, &agent_ed).await.unwrap();
        let cx = crypto::derive_keys(&[0x33; 32]).x25519_pub;
        state.store.upsert_pending_client(&ns_hex, &client_ed, &cx, "", "ios").await.unwrap();
        state.store.apply_authorize(&ns_hex, &[client_ed]).await.unwrap();

        let key = crypto::derive_pipe_key(&[0x44; 32]);
        let cid = [0x55; 32];
        let url = format!("ws://{addr}/v1/ws");

        let a = PipeConnection::connect(
            &url, &agent_sk, &agent_ed, &client_ed, &ns_raw, &cid, &key, PipeRole::Initiator,
        )
        .await
        .expect("initiator connect");
        tokio::time::sleep(Duration::from_millis(50)).await; // A pending before B
        let b = PipeConnection::connect(
            &url, &client_sk, &client_ed, &agent_ed, &ns_raw, &cid, &key, PipeRole::Responder,
        )
        .await
        .expect("responder connect");

        let (mut a_tx, mut a_rx) = a.split();
        let (mut b_tx, mut b_rx) = b.split();

        const CHUNKS: usize = 64;
        const CHUNK: usize = 16 * 1024; // 64 × 16 KiB = 1 MiB each way
        const TOTAL: usize = CHUNKS * CHUNK;

        // Both directions send at the same time…
        let a_send = tokio::spawn(async move {
            for i in 0..CHUNKS {
                a_tx.send(&vec![i as u8; CHUNK]).await.unwrap();
            }
        });
        let b_send = tokio::spawn(async move {
            for i in 0..CHUNKS {
                b_tx.send(&vec![0xFF - i as u8; CHUNK]).await.unwrap();
            }
        });
        // …while both directions receive concurrently.
        let a_recv = tokio::spawn(async move {
            let mut got = 0usize;
            while got < TOTAL {
                match a_rx.recv().await.unwrap() {
                    Some(p) => got += p.len(),
                    None => break,
                }
            }
            got
        });
        let b_recv = tokio::spawn(async move {
            let mut got = 0usize;
            while got < TOTAL {
                match b_rx.recv().await.unwrap() {
                    Some(p) => got += p.len(),
                    None => break,
                }
            }
            got
        });

        a_send.await.unwrap();
        b_send.await.unwrap();
        assert_eq!(a_recv.await.unwrap(), TOTAL, "A must receive all of B's bytes");
        assert_eq!(b_recv.await.unwrap(), TOTAL, "B must receive all of A's bytes");
    }
}
