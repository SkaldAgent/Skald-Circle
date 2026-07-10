//! skald-relay-client — standalone agent-role relay client.
//!
//! Payload-agnostic: it speaks the v2 WebSocket protocol (challenge/auth,
//! `Authorize`, store-and-forward `Message`), performs E2E seal/open with
//! per-client AES-256-GCM keys, manages anti-replay counters, pairing windows,
//! and device authorization. It never interprets the decrypted bytes: it emits
//! them via [`events::RelayEvent`] and lets the application layer (the
//! `plugin-mobile-connector` crate) apply JSON semantics. See the
//! "Crate split" section of `docs/plugins/mobile-connector.md` and
//! `docs/relay/`.
//!
//! Module map:
//! - `config`   — `RelayClientConfig` + `SeedSource`
//! - `events`   — `RelayEvent` (broadcast)
//! - `identity` — seed + derived keys + namespace_id
//! - `db`       — `relay_clients` table (devices + anti-replay counters)
//! - `pairing`  — in-memory pairing sessions + QR payload
//! - `state`    — `RelayState` (networking-only: seal/open, counters, send/recv)
//! - `ws`       — the permanent reconnecting agent WebSocket (v2 binary)
//! - `pipe`     — pipe data plane: `PipeConnection` + `IncomingPipe` (pipe.md)
//! - `client`   — `RelayClient`, the public façade

pub mod client;
pub mod config;
pub mod db;
pub mod events;
pub mod identity;
pub mod pairing;
pub mod pipe;
mod state;
mod ws;

pub use client::RelayClient;
pub use config::{RelayClientConfig, SeedSource};
pub use db::{ClientRow, ClientState};
pub use events::RelayEvent;
pub use identity::Identity;
pub use pairing::{QrCodeData, SessionState, StartedPairing};
pub use pipe::{IncomingPipe, PipeConnection, PipeReceiver, PipeRole, PipeSender};
