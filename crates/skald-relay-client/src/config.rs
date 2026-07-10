//! Configuration for [`crate::RelayClient`].

use std::path::PathBuf;

/// Configuration snapshot passed to `RelayClient::new`.
///
/// `relay_url` may be empty: the client then stays idle (no WS loop is
/// spawned), which keeps the plugin toggleable without a relay configured.
pub struct RelayClientConfig {
    /// `wss://` URL of the relay (e.g. `wss://relay.skaldagent.net/v1/ws`).
    /// Empty => the client is idle.
    pub relay_url: String,
    /// Default pairing TTL in seconds (used when `start_pairing(0)` is called).
    pub pairing_ttl: u32,
    /// Where the agent's 32-byte identity seed comes from (crypto.md §9).
    pub seed: SeedSource,
}

/// Source of the persistent identity seed (crypto.md §9).
///
/// `Path` preserves an existing on-disk identity: the plugin passes
/// `Path("data/relay/seed")` — the same relative path as today — so no device
/// is orphaned on upgrade and the namespace id is unchanged. `Bytes` is for
/// tests / in-memory identities.
pub enum SeedSource {
    /// A raw 32-byte seed (tests, in-memory).
    Bytes([u8; 32]),
    /// Load (or generate + persist `0600`) the seed at the given path. The
    /// parent directory is created on first use.
    Path(PathBuf),
}
