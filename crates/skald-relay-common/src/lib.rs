//! Shared building blocks for the Skald Remote Control relay and the
//! mobile-connector plugin (see data/ios-app/plugin.md §1.1).
//!
//! - [`proto`]: protobuf-generated types for the v2 binary wire protocol
//!   (data/iOS-app/v2/relay-protocol.md §2). The relay and the mobile
//!   connector both use [`proto::v2`] to speak the same byte-level frames.
//! - [`crypto`]: domain constants, namespace derivation, challenge sign/verify,
//!   X25519 ECDH, HKDF, AES-256-GCM seal/open, and nonce/AAD construction.
//!
//! This crate has **no** dependency on Skald, axum or tokio: both the relay and
//! the plugin link it so they can never diverge from the protocol or from the
//! interop vectors in test-vectors.md.

pub mod crypto;
pub mod pipe;
pub mod proto;
