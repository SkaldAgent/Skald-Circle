//! Events broadcast by the relay client. Consumers (the plugin's
//! `RelayApp::run_event_loop`) subscribe via `RelayClient::events()`.

/// One event emitted by the relay client.
///
/// `Message.payload` is already **decrypted AND decompressed**: the client
/// peels off the v2 `version‖comp‖json` framing before emission, so the
/// consumer sees only clean bytes and applies its own JSON semantics. This is
/// the payload-agnostic boundary (see the "Crate split" section of
/// `docs/plugins/mobile-connector.md`).
#[derive(Debug, Clone)]
pub enum RelayEvent {
    /// The WS handshake completed and the relay verified our namespace_id.
    Connected,
    /// The WS connection dropped. The client will reconnect with backoff.
    Disconnected,
    /// An inbound `message` from a client, decoded end-to-end. `from` is the
    /// sender's ed25519 pubkey; `live` mirrors the wire flag.
    Message {
        from: [u8; 32],
        payload: Vec<u8>,
        live: bool,
    },
    /// A device paired (pending authorization). The consumer decides whether
    /// to call `client.authorize(ed)` (auto) or to notify the human (manual).
    ClientPaired {
        ed25519_pub: [u8; 32],
        x25519_pub: [u8; 32],
        platform: String,
    },
    /// A device was revoked via `client.revoke` or removed by `client.clear_all`.
    ClientRevoked { ed25519_pub: [u8; 32] },
}
