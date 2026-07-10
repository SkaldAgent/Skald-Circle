//! Normative quotas, timeouts and thresholds (relay-protocol.md §9, relay.md)
//! plus a fixed-window rate limiter. The values here are the spec's reasonable
//! defaults; the relay may expose them via config later.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Maximum size of a WebSocket frame (64 KiB). Above this → `payload_too_large`.
/// Applies to all pre-auth frames and to post-auth frames whose `Message` does
/// not set the `live` flag (v2 spec §5).
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// v2: max size of a WS binary frame carrying a `Message{live:true}` on an
/// authenticated connection (relay-protocol.md §5: `MAX_LIVE_FRAME_BYTES =
/// 524288`, i.e. 512 KiB exactly). The relay enforces this manually in `ws.rs`
/// based on auth state + `Message.live`.
pub const MAX_LIVE_FRAME_BYTES: usize = 512 * 1024;
/// axum's per-message cap: must be at least `MAX_LIVE_FRAME_BYTES` so live
/// frames can flow. The relay enforces the strict per-frame limits itself
/// in `ws.rs` (64 KiB pre-auth, 64 KiB post-auth non-live, 512 KiB
/// post-auth-live).
pub const TRANSPORT_FRAME_CAP: usize = MAX_LIVE_FRAME_BYTES;

/// Time allowed to receive `auth` after the `challenge`.
pub const CHALLENGE_TIMEOUT_SECS: u64 = 30;
/// No traffic for this long → close the connection.
pub const IDLE_TIMEOUT_SECS: u64 = 120;
/// Keepalive ping interval.
pub const PING_INTERVAL_SECS: u64 = 30;

/// Max queued messages per recipient; above this → `queue_full`.
pub const QUEUE_MAX_PER_DEST: i64 = 200;
/// Threshold (bytes of base64(ciphertext)) at/below which we do content-in-push.
pub const CONTENT_PUSH_MAX_B64: usize = 3500;

/// TTL for the store-and-forward queue and for idle namespaces.
pub const TTL_DAYS: i64 = 7;

/// Pairing window TTL.
pub const PAIRING_TTL_DEFAULT: u64 = 300;
pub const PAIRING_TTL_MAX: u64 = 600;

/// Anti-flood quotas on the public endpoint.
pub const IP_NEW_CONN_PER_MIN: u32 = 30;
pub const CONN_MSG_PER_MIN: u32 = 60;

// ---------------------------------------------------------------------------
// Pipe data plane (docs/relay/pipe.md §2.3). The relay becomes a stateful
// connection proxy for `/v1/pipe`; these bound its resource use. All are
// overridable via `RELAY_PIPE_*` env vars (see config.rs).
// ---------------------------------------------------------------------------

/// First side dialed, second never showed → reap the half-open pending.
pub const PIPE_PENDING_TTL_SECS: u64 = 30;
/// No bytes for this long on a matched pipe → close (reclaim dead pipes).
pub const PIPE_IDLE_TIMEOUT_SECS: u64 = 120;
/// Max concurrent matched/pending pipes per namespace.
pub const PIPE_MAX_PER_NS: usize = 8;
/// Max size of one data-plane WS binary frame (bulk transfer; separate from the
/// message-channel caps).
pub const PIPE_MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Per-connection bandwidth cap in bytes/sec, **per direction**. `0` = unlimited.
pub const PIPE_MAX_BPS_DEFAULT: u64 = 0;

/// Thread-safe fixed-window rate limiter, generic over the key.
///
/// One `allow()` per event: returns `false` when the current window's quota is
/// exceeded. The window resets automatically once it elapses.
pub struct FixedWindow<K: Eq + Hash + Clone> {
    window: Duration,
    max: u32,
    map: Mutex<HashMap<K, (Instant, u32)>>,
}

impl<K: Eq + Hash + Clone> FixedWindow<K> {
    pub fn new(window: Duration, max: u32) -> Self {
        Self {
            window,
            max,
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Record an event for `key`. `true` = allowed, `false` = quota exceeded.
    pub fn allow(&self, key: &K) -> bool {
        let mut map = self.map.lock().unwrap();
        let now = Instant::now();
        let entry = map.entry(key.clone()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.max
    }

    /// Opportunistic pruning of expired windows (called by the GC task).
    pub fn prune(&self) {
        let mut map = self.map.lock().unwrap();
        let now = Instant::now();
        map.retain(|_, (start, _)| now.duration_since(*start) < self.window);
    }
}

/// Per-connection (non-shared) rate counter: messages per minute.
pub struct ConnRate {
    window_start: Instant,
    count: u32,
}

impl ConnRate {
    pub fn new() -> Self {
        Self {
            window_start: Instant::now(),
            count: 0,
        }
    }

    /// `true` if under quota, `false` if the connection exceeded `CONN_MSG_PER_MIN`.
    pub fn allow_message(&mut self) -> bool {
        let now = Instant::now();
        if now.duration_since(self.window_start) >= Duration::from_secs(60) {
            self.window_start = now;
            self.count = 0;
        }
        self.count += 1;
        self.count <= CONN_MSG_PER_MIN
    }
}

impl Default for ConnRate {
    fn default() -> Self {
        Self::new()
    }
}
