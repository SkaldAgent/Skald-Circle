//! `/v1/pipe` data plane: the relay as a **stateful connection proxy**
//! (docs/relay/pipe.md §2). The relay never reads pipe payloads — it
//! authenticates each side (signature + namespace membership + cross-dest),
//! matches the two sides by `connection_id`, then splices opaque ciphertext
//! frames bidirectionally with a per-direction bandwidth cap.
//!
//! State machine per socket: `challenge → pipe_auth → pending → matched →
//! streaming → teardown`. The **first** side to authenticate parks in the
//! registry until the second arrives (within `pending_ttl`); the **second**
//! hands its socket halves to the first, which then owns the bidirectional
//! splice for the pipe's lifetime. Either side closing/erroring tears down both
//! (no orphans).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message as WsMsg, WebSocket};
use futures_util::stream::{SplitSink, SplitStream, StreamExt};
use futures_util::SinkExt;
use rand::RngCore;
use skald_relay_common::crypto;
use skald_relay_common::pipe::{self, PipeAuth, PipeChallenge};
use tokio::sync::oneshot;

use crate::AppState;
use crate::config::PipeConfig;
use crate::limits::CHALLENGE_TIMEOUT_SECS;

type WsSink = SplitSink<WebSocket, WsMsg>;
type WsStream = SplitStream<WebSocket>;

/// The authenticated identity of one pipe side.
#[derive(Clone, Copy)]
struct PeerMeta {
    /// This side's ed25519 pubkey.
    pubkey: [u8; 32],
    /// `SHA256(intended counterparty pubkey)`.
    dest: [u8; 32],
}

/// The second side's socket halves, handed to the first side once the relay has
/// verified the cross-dest match. Identity was checked before the handoff, so
/// only the halves travel.
struct PeerArrival {
    sink: WsSink,
    stream: WsStream,
}

/// A half-open pipe: the first side authenticated, waiting for the second.
struct PendingPipe {
    ns: String,
    meta: PeerMeta,
    /// The first side awaits this; the second side sends its halves through it.
    peer_tx: oneshot::Sender<PeerArrival>,
}

/// Why an insert was refused.
#[derive(Debug)]
enum InsertError {
    /// `connection_id` already has a pending side.
    Duplicate,
    /// The namespace is at its concurrent-pipe cap.
    TooMany,
}

/// In-memory pipe registry shared across all `/v1/pipe` connection tasks.
#[derive(Default)]
pub struct PipeRegistry {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// keyed by `connection_id` hex.
    pending: HashMap<String, PendingPipe>,
    /// namespace_id hex → number of active pipes (pending + matched). Each pipe
    /// is counted once (by its first side) for its whole lifetime.
    counts: HashMap<String, usize>,
}

impl PipeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the first side. Increments the namespace pipe count on success;
    /// the caller MUST call [`release`](Self::release) exactly once when done.
    fn try_insert(
        &self,
        cid_hex: &str,
        pending: PendingPipe,
        max_per_ns: usize,
    ) -> Result<(), InsertError> {
        let mut g = self.inner.lock().unwrap();
        if g.pending.contains_key(cid_hex) {
            return Err(InsertError::Duplicate);
        }
        let count = g.counts.get(&pending.ns).copied().unwrap_or(0);
        if count >= max_per_ns {
            return Err(InsertError::TooMany);
        }
        *g.counts.entry(pending.ns.clone()).or_insert(0) += 1;
        g.pending.insert(cid_hex.to_string(), pending);
        Ok(())
    }

    /// Take a pending side (the second arrival claims it). Does NOT touch the
    /// count — that is released by the first side.
    fn take(&self, cid_hex: &str) -> Option<PendingPipe> {
        self.inner.lock().unwrap().pending.remove(cid_hex)
    }

    /// Release the first side: drop any lingering pending entry for `cid_hex`
    /// and decrement the namespace count. Call exactly once per [`try_insert`].
    fn release(&self, cid_hex: &str, ns: &str) {
        let mut g = self.inner.lock().unwrap();
        g.pending.remove(cid_hex);
        if let Some(c) = g.counts.get_mut(ns) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                g.counts.remove(ns);
            }
        }
    }
}

/// Drive one accepted `/v1/pipe` WebSocket to completion (called from `lib.rs`
/// after the axum upgrade).
pub async fn handle_pipe_socket(socket: WebSocket, state: AppState, peer_ip: IpAddr) {
    let (mut sink, mut stream) = socket.split();
    let cfg = state.cfg.pipe.clone();

    // 1. Challenge: the relay speaks first (mirrors the main WS, pipe.md §2.1).
    let mut nonce = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    let chal = PipeChallenge { nonce: nonce.to_vec() };
    if sink.send(WsMsg::Binary(pipe::encode(&chal).into())).await.is_err() {
        return;
    }

    // 2. Read the auth frame within the challenge timeout.
    let Some(auth) =
        read_pipe_auth(&mut stream, Duration::from_secs(CHALLENGE_TIMEOUT_SECS), cfg.max_frame_bytes)
            .await
    else {
        return;
    };

    // 3. Validate field lengths.
    let (Some(cid), Some(pubkey), Some(dest), Some(ns_raw), Some(sig)) = (
        pipe::to_array::<32>(&auth.connection_id),
        pipe::to_array::<32>(&auth.pubkey),
        pipe::to_array::<32>(&auth.dest),
        pipe::to_array::<32>(&auth.namespace_id),
        pipe::to_array::<64>(&auth.signature),
    ) else {
        return close(&mut sink, "bad_request").await;
    };

    // 3a. Signature proves control of `pubkey` and binds nonce + connection_id.
    if !crypto::verify_pipe_auth(&pubkey, &nonce, &cid, &sig) {
        return close(&mut sink, "invalid_signature").await;
    }
    // 3b. Namespace membership: the agent, or an authorized client.
    let ns = hex::encode(ns_raw);
    if !is_member(&state, &ns, &pubkey).await {
        return close(&mut sink, "unauthorized").await;
    }

    let cid_hex = hex::encode(cid);
    let meta = PeerMeta { pubkey, dest };

    // 4. Rendezvous by connection_id.
    if let Some(pending) = state.pipes.take(&cid_hex) {
        // We are the SECOND side: verify cross-refs, then hand our halves over.
        let cross_ok = pending.ns == ns
            && crypto::sha256(&pending.meta.pubkey) == dest
            && crypto::sha256(&pubkey) == pending.meta.dest;
        if !cross_ok {
            // Dropping `pending.peer_tx` also unblocks + tears down the first side.
            tracing::debug!(target: "relay::pipe", ns = %short(&ns), %peer_ip, "cross-dest mismatch");
            return close(&mut sink, "not_found").await;
        }
        let arrival = PeerArrival { sink, stream };
        if pending.peer_tx.send(arrival).is_err() {
            tracing::debug!(target: "relay::pipe", ns = %short(&ns), "first side gone before match");
        }
        // The first side now owns the splice; our halves moved into it.
        return;
    }

    // We are the FIRST side: register and park until the second arrives.
    let (peer_tx, peer_rx) = oneshot::channel::<PeerArrival>();
    let pending = PendingPipe { ns: ns.clone(), meta, peer_tx };
    match state.pipes.try_insert(&cid_hex, pending, cfg.max_per_ns) {
        Ok(()) => {}
        Err(InsertError::Duplicate) => return close(&mut sink, "duplicate_connection").await,
        Err(InsertError::TooMany) => return close(&mut sink, "too_many_pipes").await,
    }

    let ttl = Duration::from_secs(cfg.pending_ttl_secs);
    match tokio::time::timeout(ttl, peer_rx).await {
        Ok(Ok(arrival)) => {
            tracing::info!(target: "relay::pipe", ns = %short(&ns), %peer_ip, "pipe matched; streaming");
            splice(sink, stream, arrival.sink, arrival.stream, &cfg).await;
        }
        Ok(Err(_)) => {
            // Second side dropped its sender (closed / cross-dest mismatch).
            let _ = close(&mut sink, "peer_aborted").await;
        }
        Err(_) => {
            tracing::debug!(target: "relay::pipe", ns = %short(&ns), "pending TTL expired");
            let _ = close(&mut sink, "timeout").await;
        }
    }
    state.pipes.release(&cid_hex, &ns);
}

/// `true` if `pubkey` is the agent of `ns` or an authorized client.
async fn is_member(state: &AppState, ns: &str, pubkey: &[u8; 32]) -> bool {
    if matches!(state.store.agent_pub(ns).await, Ok(Some(a)) if &a == pubkey) {
        return true;
    }
    state.store.is_authorized_client(ns, pubkey).await.unwrap_or(false)
}

/// Read binary frames until the first one decodes as [`PipeAuth`]; `None` on
/// timeout, oversize, non-binary, malformed, or early close.
async fn read_pipe_auth(
    stream: &mut WsStream,
    within: Duration,
    max_frame: usize,
) -> Option<PipeAuth> {
    let deadline = tokio::time::sleep(within);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => return None,
            msg = stream.next() => match msg {
                Some(Ok(WsMsg::Binary(data))) => {
                    if data.len() > max_frame {
                        return None;
                    }
                    return pipe::decode::<PipeAuth>(&data).ok();
                }
                Some(Ok(WsMsg::Ping(_))) | Some(Ok(WsMsg::Pong(_))) => continue,
                _ => return None, // close, error, or text (pipe is binary-only)
            }
        }
    }
}

/// What the splice loop should do after handling one frame.
enum Flow {
    Continue,
    Close,
}

/// Bidirectionally forward binary frames between the two sides until either
/// closes/errors or the pipe goes idle. WS-level Ping is answered on the
/// originating socket; data is rate-limited per direction. On exit both sides
/// are closed (no half-close in v1).
async fn splice(
    mut a_sink: WsSink,
    mut a_stream: WsStream,
    mut b_sink: WsSink,
    mut b_stream: WsStream,
    cfg: &PipeConfig,
) {
    let idle = Duration::from_secs(cfg.idle_timeout_secs);
    let mut bucket_ab = TokenBucket::new(cfg.max_bps, cfg.max_frame_bytes);
    let mut bucket_ba = TokenBucket::new(cfg.max_bps, cfg.max_frame_bytes);

    loop {
        let timeout = tokio::time::sleep(idle);
        tokio::pin!(timeout);
        tokio::select! {
            _ = &mut timeout => break,
            ma = a_stream.next() => {
                if let Flow::Close =
                    forward(ma, &mut b_sink, &mut a_sink, &mut bucket_ab, cfg.max_frame_bytes).await
                {
                    break;
                }
            }
            mb = b_stream.next() => {
                if let Flow::Close =
                    forward(mb, &mut a_sink, &mut b_sink, &mut bucket_ba, cfg.max_frame_bytes).await
                {
                    break;
                }
            }
        }
    }
    let _ = a_sink.send(WsMsg::Close(None)).await;
    let _ = b_sink.send(WsMsg::Close(None)).await;
}

/// Handle one inbound frame from a side: forward `Binary` to `to_sink` (rate
/// limited), answer `Ping` on `same_sink`, ignore `Pong`, and treat
/// close/error/text/oversize as end-of-pipe.
async fn forward(
    msg: Option<Result<WsMsg, axum::Error>>,
    to_sink: &mut WsSink,
    same_sink: &mut WsSink,
    bucket: &mut Option<TokenBucket>,
    max_frame: usize,
) -> Flow {
    let Some(Ok(m)) = msg else { return Flow::Close };
    match m {
        WsMsg::Binary(data) => {
            if data.len() > max_frame {
                return Flow::Close;
            }
            if let Some(b) = bucket {
                b.consume(data.len()).await;
            }
            if to_sink.send(WsMsg::Binary(data)).await.is_err() {
                return Flow::Close;
            }
            Flow::Continue
        }
        WsMsg::Ping(p) => {
            let _ = same_sink.send(WsMsg::Pong(p)).await;
            Flow::Continue
        }
        WsMsg::Pong(_) => Flow::Continue,
        // Close, or a text frame (pipe is binary-only) → tear down.
        _ => Flow::Close,
    }
}

/// Send a debug-logged close. The data plane has no error frame; the client
/// reads a close during/after the handshake as a rejection.
async fn close(sink: &mut WsSink, reason: &str) {
    tracing::debug!(target: "relay::pipe", reason, "closing pipe socket");
    let _ = sink.send(WsMsg::Close(None)).await;
}

/// Truncate a namespace_id for logging.
fn short(s: &str) -> String {
    let n = s.len().min(8);
    format!("{}…", &s[..n])
}

/// Simple token bucket for the per-direction byte-rate cap. `None` (via
/// [`TokenBucket::new`] with `max_bps == 0`) means unlimited. Burst is bounded
/// to `max(rate, frame_cap)` so a single max-size frame can always pass.
struct TokenBucket {
    rate: f64,
    burst: f64,
    allowance: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(max_bps: u64, frame_cap: usize) -> Option<TokenBucket> {
        if max_bps == 0 {
            return None;
        }
        let rate = max_bps as f64;
        let burst = rate.max(frame_cap as f64);
        Some(TokenBucket { rate, burst, allowance: burst, last: Instant::now() })
    }

    /// Block until `bytes` worth of tokens are available, then deduct them.
    async fn consume(&mut self, bytes: usize) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.allowance = (self.allowance + elapsed * self.rate).min(self.burst);
        let need = bytes as f64;
        if self.allowance < need {
            let wait = (need - self.allowance) / self.rate;
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            self.allowance = 0.0;
        } else {
            self.allowance -= need;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_enforces_per_ns_cap_and_releases() {
        let reg = PipeRegistry::new();
        let meta = PeerMeta { pubkey: [1; 32], dest: [2; 32] };
        let mk = |ns: &str| {
            let (tx, _rx) = oneshot::channel();
            PendingPipe { ns: ns.into(), meta, peer_tx: tx }
        };
        assert!(reg.try_insert("a", mk("ns"), 2).is_ok());
        assert!(reg.try_insert("b", mk("ns"), 2).is_ok());
        // Third in the same ns is over the cap.
        assert!(matches!(reg.try_insert("c", mk("ns"), 2), Err(InsertError::TooMany)));
        // Duplicate connection_id is refused regardless of cap.
        assert!(matches!(reg.try_insert("a", mk("ns"), 9), Err(InsertError::Duplicate)));
        // Releasing one frees a slot.
        reg.release("a", "ns");
        assert!(reg.try_insert("c", mk("ns"), 2).is_ok());
    }

    #[test]
    fn take_returns_pending_once() {
        let reg = PipeRegistry::new();
        let (tx, _rx) = oneshot::channel();
        let meta = PeerMeta { pubkey: [1; 32], dest: [2; 32] };
        reg.try_insert("x", PendingPipe { ns: "ns".into(), meta, peer_tx: tx }, 4).unwrap();
        assert!(reg.take("x").is_some());
        assert!(reg.take("x").is_none());
        reg.release("x", "ns");
    }

    #[tokio::test]
    async fn token_bucket_unlimited_is_noop() {
        assert!(TokenBucket::new(0, 1024).is_none());
    }

    #[tokio::test]
    async fn token_bucket_passes_large_frame() {
        // A frame bigger than the per-second rate must still go through (burst
        // is bounded to the frame cap), just after a bounded wait.
        let mut b = TokenBucket::new(1000, 4096).unwrap();
        b.consume(4096).await; // initial burst
        b.consume(4096).await; // forces a wait, then succeeds
    }
}
