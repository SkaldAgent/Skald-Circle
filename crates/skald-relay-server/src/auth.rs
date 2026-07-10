//! The relay's cryptographic operations: verifying the Ed25519 challenge
//! signature (crypto.md §8) and deriving the `namespace_id` (crypto.md §7).
//!
//! The implementation now lives in the shared `skald-relay-common` crate so the
//! relay and the mobile-connector plugin can never diverge (see plugin.md §1.1).
//! The relay uses only the verify/namespace subset; the full E2E suite is end-to-
//! end between agent and client and not touched here. Re-exported so existing
//! relay paths (`crate::auth::…`) keep working unchanged.

pub use skald_relay_common::crypto::{
    AUTH_DOMAIN, NS_DOMAIN, ct_eq, decode_hex, namespace_id, verify_challenge,
};
