//! In-memory registry of live connections (relay.md §4). Maps each namespace to
//! its single agent connection and its set of client connections. Used to
//! forward messages live; when the recipient is absent the caller falls back to
//! store-and-forward + push.
//!
//! Concurrency: a plain `std::sync::Mutex` guards the map. We never hold the
//! lock across an `.await`: lookups clone the cheap `mpsc::Sender` and release
//! the lock before sending. Stale-connection eviction uses a per-connection
//! `CancellationToken` plus a unique id so a connection only ever removes its
//! own entry.

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::types::proto::RelayFrame;

/// Items sent to a connection's writer task (the task that owns the WS sink).
///
/// v2 transport: every control/data frame is a protobuf [`RelayFrame`] carried
/// inside a WebSocket **binary** message. WS-level Ping/Pong are used for
/// keepalive (relay-protocol.md §5) and are therefore their own variants so
/// the writer does not have to encode them as protobuf.
pub enum WsOut {
    /// A protobuf `RelayFrame` to be encoded and sent as a WebSocket **binary** frame.
    Frame(RelayFrame),
    /// A WS-level Pong (reply to an inbound WS Ping).
    Pong(Vec<u8>),
    /// A WS-level Ping (keepalive). Payload is opaque; a 0-byte payload is fine.
    Ping(Vec<u8>),
    /// Ask the writer to close the socket (eviction / fatal error).
    Close,
}

/// A handle to one live WebSocket's writer task.
#[derive(Clone)]
pub struct ConnHandle {
    /// Unique id of the connection (identity check on self-removal).
    pub id: u64,
    /// Sender into the connection's writer task.
    pub tx: mpsc::Sender<WsOut>,
    /// Cancels the connection (used to evict a replaced/revoked peer).
    pub cancel: CancellationToken,
    /// ed25519 pubkey of the peer authenticated on this connection. Agents and
    /// clients both have one; used to build `PresenceList.online[]` and to
    /// populate `PresenceEvent.pubkey` (v2 spec §4).
    pub pubkey: [u8; 32],
}

#[derive(Default)]
struct NamespaceConns {
    /// The single agent connection for this namespace, if any. The agent's
    /// pubkey lives on the [`ConnHandle`].
    agent: Option<ConnHandle>,
    /// keyed by client ed25519 pubkey, hex.
    clients: HashMap<String, ConnHandle>,
}

/// Thread-safe registry shared across all connection tasks.
#[derive(Default)]
pub struct Registry {
    inner: Mutex<HashMap<String, NamespaceConns>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the agent connection for `ns`, returning the previous one (if
    /// any) so the caller can cancel it (one agent per namespace).
    pub fn register_agent(&self, ns: &str, handle: ConnHandle) -> Option<ConnHandle> {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(ns.to_string()).or_default();
        entry.agent.replace(handle)
    }

    /// Register a client connection, returning the previous one for the same
    /// pubkey (if any) so the caller can cancel it (one connection per device).
    pub fn register_client(
        &self,
        ns: &str,
        pubkey_hex: &str,
        handle: ConnHandle,
    ) -> Option<ConnHandle> {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry(ns.to_string()).or_default();
        entry.clients.insert(pubkey_hex.to_string(), handle)
    }

    /// Live sender of the namespace's agent, if connected.
    pub fn agent_tx(&self, ns: &str) -> Option<mpsc::Sender<WsOut>> {
        let map = self.inner.lock().unwrap();
        map.get(ns)
            .and_then(|c| c.agent.as_ref())
            .map(|h| h.tx.clone())
    }

    /// Live sender of a client, if connected.
    pub fn client_tx(&self, ns: &str, pubkey_hex: &str) -> Option<mpsc::Sender<WsOut>> {
        let map = self.inner.lock().unwrap();
        map.get(ns)
            .and_then(|c| c.clients.get(pubkey_hex))
            .map(|h| h.tx.clone())
    }

    /// Remove the agent entry, but only if it is still the connection with `id`.
    pub fn remove_agent(&self, ns: &str, id: u64) {
        let mut map = self.inner.lock().unwrap();
        if let Some(conns) = map.get_mut(ns) {
            if conns.agent.as_ref().is_some_and(|h| h.id == id) {
                conns.agent = None;
            }
            Self::gc_empty(&mut map, ns);
        }
    }

    /// Remove a client entry, but only if it is still the connection with `id`.
    pub fn remove_client(&self, ns: &str, pubkey_hex: &str, id: u64) {
        let mut map = self.inner.lock().unwrap();
        if let Some(conns) = map.get_mut(ns) {
            if conns.clients.get(pubkey_hex).is_some_and(|h| h.id == id) {
                conns.clients.remove(pubkey_hex);
            }
            Self::gc_empty(&mut map, ns);
        }
    }

    /// Evict a client by pubkey regardless of id (revocation). Returns the
    /// handle so the caller can cancel it.
    pub fn evict_client(&self, ns: &str, pubkey_hex: &str) -> Option<ConnHandle> {
        let mut map = self.inner.lock().unwrap();
        let handle = map.get_mut(ns).and_then(|c| c.clients.remove(pubkey_hex));
        Self::gc_empty(&mut map, ns);
        handle
    }

    /// All pubkeys currently connected in `ns`: the agent (if connected)
    /// followed by every connected client. Used to build
    /// `PresenceList.online[]` in response to `PresenceRequest` (v2 spec §4).
    pub fn list_online(&self, ns: &str) -> Vec<[u8; 32]> {
        let map = self.inner.lock().unwrap();
        let Some(conns) = map.get(ns) else {
            return Vec::new();
        };
        let mut out: Vec<[u8; 32]> = Vec::with_capacity(1 + conns.clients.len());
        if let Some(h) = &conns.agent {
            out.push(h.pubkey);
        }
        for (_, h) in &conns.clients {
            out.push(h.pubkey);
        }
        out
    }

    /// Broadcast `frame` to every connection in `ns`, optionally skipping the
    /// connection with `id == skip_id`. Used for `PresenceEvent` (skip the
    /// source so it doesn't see its own presence change).
    ///
    /// Errors are silently dropped: a slow/blocked peer must not stall the
    /// sender while we hold the registry mutex. If the channel is full the
    /// frame is dropped for that peer — acceptable for presence (the peer
    /// will see the next periodic refresh or a later event).
    ///
    /// Returns the number of targets the frame was **offered** to (i.e.
    /// `try_send` did not fail because the channel was closed). Returns 0 if
    /// the namespace is unknown.
    pub fn broadcast_ns(&self, ns: &str, frame: RelayFrame, skip_id: Option<u64>) -> usize {
        let map = self.inner.lock().unwrap();
        let Some(conns) = map.get(ns) else {
            return 0;
        };
        let mut n = 0usize;
        if let Some(h) = &conns.agent
            && skip_id != Some(h.id)
        {
            if h.tx.try_send(WsOut::Frame(frame.clone())).is_ok() {
                n += 1;
            }
        }
        for (_, h) in &conns.clients {
            if skip_id == Some(h.id) {
                continue;
            }
            if h.tx.try_send(WsOut::Frame(frame.clone())).is_ok() {
                n += 1;
            }
        }
        n
    }

    fn gc_empty(map: &mut HashMap<String, NamespaceConns>, ns: &str) {
        if let Some(conns) = map.get(ns)
            && conns.agent.is_none()
            && conns.clients.is_empty()
        {
            map.remove(ns);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle(id: u64, pubkey: [u8; 32]) -> (ConnHandle, mpsc::Receiver<WsOut>) {
        let (tx, rx) = mpsc::channel(4);
        (
            ConnHandle {
                id,
                tx,
                cancel: CancellationToken::new(),
                pubkey,
            },
            rx,
        )
    }

    #[test]
    fn agent_replacement_returns_old() {
        let reg = Registry::new();
        let (h1, _r1) = handle(1, [0xAA; 32]);
        let (h2, _r2) = handle(2, [0xBB; 32]);
        assert!(reg.register_agent("ns", h1).is_none());
        let old = reg.register_agent("ns", h2).expect("old agent");
        assert_eq!(old.id, 1);
        assert!(reg.agent_tx("ns").is_some());
    }

    #[test]
    fn self_removal_respects_identity() {
        let reg = Registry::new();
        let (h1, _r1) = handle(1, [0xAA; 32]);
        let (h2, _r2) = handle(2, [0xBB; 32]);
        reg.register_agent("ns", h1);
        // A newer connection replaced id=1 with id=2.
        reg.register_agent("ns", h2);
        // The old connection (id=1) cleaning up must NOT drop the new one.
        reg.remove_agent("ns", 1);
        assert!(reg.agent_tx("ns").is_some());
        // The current connection (id=2) removes itself → gone.
        reg.remove_agent("ns", 2);
        assert!(reg.agent_tx("ns").is_none());
    }

    #[test]
    fn evict_client_returns_handle() {
        let reg = Registry::new();
        let (h, _r) = handle(7, [0xCC; 32]);
        reg.register_client("ns", "ab", h);
        assert!(reg.client_tx("ns", "ab").is_some());
        let evicted = reg.evict_client("ns", "ab").expect("handle");
        assert_eq!(evicted.id, 7);
        assert!(reg.client_tx("ns", "ab").is_none());
    }

    #[test]
    fn list_online_returns_agent_and_clients() {
        let reg = Registry::new();
        let agent_pub = [0xAAu8; 32];
        let client_pub = [0xBBu8; 32];
        let (h1, _r1) = handle(1, agent_pub);
        let (h2, _r2) = handle(2, client_pub);
        reg.register_agent("ns", h1);
        reg.register_client("ns", &hex::encode(client_pub), h2);
        let online = reg.list_online("ns");
        assert_eq!(online.len(), 2);
        assert!(online.contains(&agent_pub));
        assert!(online.contains(&client_pub));
    }

    #[test]
    fn list_online_empty_when_namespace_unknown() {
        let reg = Registry::new();
        assert!(reg.list_online("nope").is_empty());
    }

    #[test]
    fn list_online_agent_only_when_no_clients() {
        let reg = Registry::new();
        let agent_pub = [0xAAu8; 32];
        let (h, _r) = handle(1, agent_pub);
        reg.register_agent("ns", h);
        let online = reg.list_online("ns");
        assert_eq!(online, vec![agent_pub]);
    }

    #[test]
    fn broadcast_ns_skips_source() {
        let reg = Registry::new();
        let (h1, mut r1) = handle(1, [0xAA; 32]);
        let (h2, mut r2) = handle(2, [0xBB; 32]);
        reg.register_agent("ns", h1);
        reg.register_client("ns", &hex::encode([0xBB; 32]), h2);
        let frame = RelayFrame { frame: None };
        let n = reg.broadcast_ns("ns", frame, Some(1)); // skip id=1 (agent)
        assert_eq!(n, 1);
        // Agent (id=1) should NOT see the frame.
        assert!(r1.try_recv().is_err());
        // Client (id=2) should see it.
        assert!(r2.try_recv().is_ok());
    }

    #[test]
    fn broadcast_ns_with_no_skip_targets_all() {
        let reg = Registry::new();
        let (h1, mut r1) = handle(1, [0xAA; 32]);
        let (h2, mut r2) = handle(2, [0xBB; 32]);
        reg.register_agent("ns", h1);
        reg.register_client("ns", &hex::encode([0xBB; 32]), h2);
        let frame = RelayFrame { frame: None };
        let n = reg.broadcast_ns("ns", frame, None);
        assert_eq!(n, 2);
        assert!(r1.try_recv().is_ok());
        assert!(r2.try_recv().is_ok());
    }

    #[test]
    fn broadcast_ns_unknown_namespace_returns_zero() {
        let reg = Registry::new();
        let frame = RelayFrame { frame: None };
        assert_eq!(reg.broadcast_ns("nope", frame, None), 0);
    }
}
