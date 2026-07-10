//! Agent identity: the 32-byte seed and the keys derived from it.
//!
//! The seed is the only persistent secret (crypto.md §9). When sourced from a
//! path it lives on the filesystem with `0600` permissions and is generated on
//! first use. All Ed25519 / X25519 key material and the `namespace_id` are
//! derived from it at runtime via `skald-relay-common` (crypto.md §3-7), never
//! persisted — so the byte-for-byte interop with the reference vectors is
//! inherited from the shared crate.
//!
//! The seed location is supplied by the caller via [`SeedSource`] (no more
//! CWD-coupled `const`). The plugin passes `SeedSource::Path("data/relay/seed")`
//! to preserve the existing identity on upgrade.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::RngCore;
use skald_relay_common::crypto::{self, DerivedKeys};

use crate::config::SeedSource;

/// The agent's cryptographic identity, derived from the persistent seed.
pub struct Identity {
    keys: DerivedKeys,
    /// `namespace_id` raw 32 bytes (used to build the AEAD AAD).
    ns_raw: [u8; 32],
    /// `namespace_id` lowercase hex (64 chars), used on the wire.
    ns_hex: String,
}

impl Identity {
    /// Build an identity from a [`SeedSource`]: for `Bytes` this is pure
    /// in-memory derivation; for `Path` it loads (or generates + persists
    /// `0600`) the seed at the given path, preserving any existing identity.
    pub fn from_source(source: &SeedSource) -> Result<Self> {
        match source {
            SeedSource::Bytes(seed) => Ok(Self::from_seed(seed)),
            SeedSource::Path(p) => {
                let seed = load_or_create_seed(p)?;
                Ok(Self::from_seed(&seed))
            }
        }
    }

    /// Build an identity from a raw seed (used by `from_source` and tests).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let keys = crypto::derive_keys(seed);
        let (ns_raw, ns_hex) = crypto::namespace_id(&keys.ed25519_pub);
        Self { keys, ns_raw, ns_hex }
    }

    pub fn ed25519_pub(&self) -> [u8; 32] {
        self.keys.ed25519_pub
    }

    pub fn x25519_pub(&self) -> [u8; 32] {
        self.keys.x25519_pub
    }

    pub fn signing_key(&self) -> ed25519_dalek::SigningKey {
        self.keys.signing_key()
    }

    pub fn namespace_id_raw(&self) -> [u8; 32] {
        self.ns_raw
    }

    pub fn namespace_id_hex(&self) -> &str {
        &self.ns_hex
    }

    /// Derive the per-client AES-256-GCM key from this agent's X25519 private key
    /// and the peer's X25519 public key (crypto.md §4-5).
    pub fn derive_aes_key(&self, client_x25519_pub: &[u8; 32]) -> [u8; 32] {
        let shared = crypto::ecdh(&self.keys.x25519_priv, client_x25519_pub);
        crypto::derive_aes_key(&shared)
    }
}

/// Read the seed, or generate a fresh 32-byte CSPRNG seed and persist it `0600`.
fn load_or_create_seed(path: &Path) -> Result<[u8; 32]> {
    if let Ok(bytes) = std::fs::read(path) {
        if bytes.len() == 32 {
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes);
            return Ok(seed);
        }
        anyhow::bail!(
            "relay seed at {} has wrong length ({}, expected 32) — refusing to overwrite",
            path.display(),
            bytes.len()
        );
    }

    // First run: create the parent dir (defaulting to "." if the path has no
    // parent, e.g. a bare filename) and persist a fresh seed.
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating seed dir {}", dir.display()))?;

    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    write_secret_file(path, &seed)
        .with_context(|| format!("writing seed file {}", path.display()))?;
    tracing::info!(
        crate_name = "skald-relay-client",
        "generated new relay seed at {}",
        path.display()
    );
    Ok(seed)
}

/// Write `bytes` to `path` with `0600` permissions on Unix.
fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_matches_reference_vectors() {
        // Same seed as skald-relay-common's pinned vector (bytes 0..32).
        let seed: [u8; 32] = (0u8..32).collect::<Vec<_>>().try_into().unwrap();
        let id = Identity::from_seed(&seed);
        assert_eq!(
            hex::encode(id.ed25519_pub()),
            "b3e202f4ac99fd9929da47df20adedd5b2598411a466a229f086eda3467ffa7b"
        );
        assert_eq!(
            id.namespace_id_hex(),
            "f7d340d3c3f0b0052fa904ba60ebd38a0f7e7d10672ac80648991a2c632c9e58"
        );
    }

    #[test]
    fn from_source_bytes_matches_from_seed() {
        let seed: [u8; 32] = (0u8..32).collect::<Vec<_>>().try_into().unwrap();
        let a = Identity::from_source(&SeedSource::Bytes(seed)).expect("bytes");
        let b = Identity::from_seed(&seed);
        assert_eq!(a.ed25519_pub(), b.ed25519_pub());
        assert_eq!(a.namespace_id_hex(), b.namespace_id_hex());
    }
}
