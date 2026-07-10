//! Cryptography shared between the relay, the mobile-connector plugin, and the
//! reference vector generator (see plugin.md §1.1, crypto.md).
//!
//! Two layers live here:
//!
//! 1. **Relay subset** — the relay only ever verifies the Ed25519
//!    challenge-response signature (crypto.md §8) and derives the `namespace_id`
//!    (crypto.md §7). It never touches X25519/AEAD.
//! 2. **End-to-end suite** — key derivation from a seed, X25519 ECDH, the
//!    HKDF → `aes_key` step, AES-256-GCM seal/open, and nonce construction
//!    (crypto.md §3-6). These are used by the plugin (and by `gen-vectors`) so
//!    that every Rust consumer produces byte-identical output to the reference
//!    vectors in test-vectors.md.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use subtle::ConstantTimeEq;
use x25519_dalek::{PublicKey, StaticSecret};

// ---------------------------------------------------------------------------
// Domain constants (crypto.md §1). These MUST stay byte-identical across all
// implementations — changing one invalidates the protocol and the vectors.
// ---------------------------------------------------------------------------

/// Challenge-response signing domain (crypto.md §1, `AUTH_DOMAIN`).
pub const AUTH_DOMAIN: &[u8] = b"skald-relay-auth-v1";
/// Namespace derivation domain (crypto.md §1, `NS_DOMAIN`).
pub const NS_DOMAIN: &[u8] = b"skald-namespace-v1";
/// Key-derivation salt for deriving Ed25519/X25519 private keys from the seed
/// (crypto.md §3). Used with info `"ed25519"` / `"x25519"`.
pub const KDF_SALT: &[u8] = b"skald-kdf-v1";
/// HKDF info string for the X25519 private key derivation (crypto.md §3).
pub const KDF_INFO_X25519: &[u8] = b"x25519";
/// HKDF info string for the Ed25519 private key derivation (crypto.md §3).
pub const KDF_INFO_ED25519: &[u8] = b"ed25519";
/// HKDF salt for deriving the per-session AES key from the ECDH shared secret
/// (crypto.md §5).
pub const SESSION_SALT: &[u8] = b"skald-session-v1";
/// HKDF info string for the AES-256-GCM session key (crypto.md §5).
pub const SESSION_INFO_AES: &[u8] = b"aes-256-gcm";

/// Nonce direction prefix, agent → client (crypto.md §6).
pub const DIR_AGENT_TO_CLIENT: [u8; 4] = [0, 0, 0, 1];
/// Nonce direction prefix, client → agent (crypto.md §6).
pub const DIR_CLIENT_TO_AGENT: [u8; 4] = [0, 0, 0, 2];

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

/// Decode a hex pubkey/signature/id (case-insensitive on input) into `N` bytes.
/// Returns `None` on malformed hex or wrong length.
pub fn decode_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != N {
        return None;
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    Some(out)
}

// ---------------------------------------------------------------------------
// Relay subset: namespace derivation + challenge verification
// ---------------------------------------------------------------------------

/// `namespace_id = hex(SHA256(NS_DOMAIN ‖ 0x00 ‖ agent_ed25519_pub))` (crypto.md §7).
/// Returns both the raw 32 bytes and the lowercase hex string.
pub fn namespace_id(agent_ed25519_pub: &[u8; 32]) -> ([u8; 32], String) {
    let mut h = Sha256::new();
    h.update(NS_DOMAIN);
    h.update([0u8]);
    h.update(agent_ed25519_pub);
    let raw: [u8; 32] = h.finalize().into();
    let hexed = hex::encode(raw);
    (raw, hexed)
}

/// Build the challenge message that is signed/verified:
/// `AUTH_DOMAIN ‖ 0x00 ‖ challenge_nonce_raw(32B)` (crypto.md §8).
pub fn challenge_message(challenge_nonce_raw: &[u8; 32]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(AUTH_DOMAIN.len() + 1 + 32);
    msg.extend_from_slice(AUTH_DOMAIN);
    msg.push(0x00);
    msg.extend_from_slice(challenge_nonce_raw);
    msg
}

/// Verify the Ed25519 challenge signature over the [`challenge_message`] under
/// `ed25519_pub`. Uses `verify_strict` (rejects malleable signatures / low-order
/// keys).
pub fn verify_challenge(
    ed25519_pub: &[u8; 32],
    challenge_nonce_raw: &[u8; 32],
    signature: &[u8; 64],
) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(ed25519_pub) else {
        return false;
    };
    let sig = Signature::from_bytes(signature);
    vk.verify_strict(&challenge_message(challenge_nonce_raw), &sig)
        .is_ok()
}

/// Sign the challenge message with `signing_key` (used by the agent/plugin and
/// by the reference generator). Returns the 64-byte signature.
pub fn sign_challenge(signing_key: &SigningKey, challenge_nonce_raw: &[u8; 32]) -> [u8; 64] {
    signing_key
        .sign(&challenge_message(challenge_nonce_raw))
        .to_bytes()
}

/// Plain SHA-256 of `data` (used e.g. for the pipe data-plane `dest =
/// SHA256(peer_ed25519_pub)` cross-check, pipe.md §2.1).
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// Constant-time comparison of two tokens/secrets (relay.md §6: pairing_token).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

// ---------------------------------------------------------------------------
// End-to-end suite: key derivation, ECDH, AEAD (crypto.md §3-6)
// ---------------------------------------------------------------------------

/// HKDF-SHA256 expanding to a 32-byte output (crypto.md §3/§5).
pub fn hkdf32(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = [0u8; 32];
    hk.expand(info, &mut out)
        .expect("32 is a valid HKDF-SHA256 output length");
    out
}

/// The Ed25519 + X25519 key material derived from a 32-byte seed (crypto.md §3).
pub struct DerivedKeys {
    pub ed25519_priv: [u8; 32],
    pub ed25519_pub: [u8; 32],
    pub x25519_priv: [u8; 32],
    pub x25519_pub: [u8; 32],
}

impl DerivedKeys {
    /// Reconstruct the Ed25519 signing key from the derived private bytes.
    pub fn signing_key(&self) -> SigningKey {
        SigningKey::from_bytes(&self.ed25519_priv)
    }

    /// Reconstruct the X25519 static secret from the derived private bytes.
    pub fn x25519_secret(&self) -> StaticSecret {
        StaticSecret::from(self.x25519_priv)
    }
}

/// Derive the Ed25519 and X25519 key pairs from a 32-byte seed (crypto.md §3):
/// each private key is `HKDF(seed, salt="skald-kdf-v1", info=<algo>, 32)`.
pub fn derive_keys(seed: &[u8; 32]) -> DerivedKeys {
    let ed25519_priv = hkdf32(seed, KDF_SALT, KDF_INFO_ED25519);
    let x25519_priv = hkdf32(seed, KDF_SALT, KDF_INFO_X25519);

    let signing = SigningKey::from_bytes(&ed25519_priv);
    let ed25519_pub = signing.verifying_key().to_bytes();

    let x_secret = StaticSecret::from(x25519_priv);
    let x25519_pub = PublicKey::from(&x_secret).to_bytes();

    DerivedKeys {
        ed25519_priv,
        ed25519_pub,
        x25519_priv,
        x25519_pub,
    }
}

/// X25519 public key from a raw 32-byte private scalar (clamping applied
/// internally, consistent with [`ecdh`]). Used to derive a pipe ephemeral pubkey
/// from a freshly-sampled private key (pipe.md §3).
pub fn x25519_pubkey(x25519_priv: &[u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(*x25519_priv)).to_bytes()
}

/// X25519 ECDH: compute the shared secret between `my_x25519_priv` and the
/// peer's public key (crypto.md §4).
pub fn ecdh(my_x25519_priv: &[u8; 32], peer_x25519_pub: &[u8; 32]) -> [u8; 32] {
    let secret = StaticSecret::from(*my_x25519_priv);
    let peer = PublicKey::from(*peer_x25519_pub);
    *secret.diffie_hellman(&peer).as_bytes()
}

/// Derive the per-session AES-256-GCM key from the ECDH shared secret
/// (crypto.md §5).
pub fn derive_aes_key(shared_secret: &[u8; 32]) -> [u8; 32] {
    hkdf32(shared_secret, SESSION_SALT, SESSION_INFO_AES)
}

/// Build the 12-byte AEAD nonce: `direction(4B) ‖ counter(8B big-endian)`
/// (crypto.md §6).
pub fn build_nonce(direction: [u8; 4], counter: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..4].copy_from_slice(&direction);
    n[4..].copy_from_slice(&counter.to_be_bytes());
    n
}

/// Build the AEAD additional-authenticated-data: `namespace_id_raw ‖ from_pub ‖
/// to_pub`, all 32-byte raw values (crypto.md §6).
pub fn build_aad(namespace_id_raw: &[u8; 32], from_pub: &[u8; 32], to_pub: &[u8; 32]) -> Vec<u8> {
    let mut aad = Vec::with_capacity(96);
    aad.extend_from_slice(namespace_id_raw);
    aad.extend_from_slice(from_pub);
    aad.extend_from_slice(to_pub);
    aad
}

/// AES-256-GCM seal: returns `ciphertext ‖ tag` (crypto.md §6). `Err` only on a
/// malformed key length (the AEAD itself never fails on encrypt).
pub fn seal(
    aes_key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(aes_key).map_err(|_| CryptoError::Key)?;
    cipher
        .encrypt(Nonce::from_slice(nonce), Payload { msg: plaintext, aad })
        .map_err(|_| CryptoError::Aead)
}

/// AES-256-GCM open: verifies the tag and returns the plaintext (crypto.md §6).
/// `Err(CryptoError::Aead)` on tag mismatch / wrong key / wrong AAD.
pub fn open(
    aes_key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = Aes256Gcm::new_from_slice(aes_key).map_err(|_| CryptoError::Key)?;
    cipher
        .decrypt(Nonce::from_slice(nonce), Payload { msg: ciphertext, aad })
        .map_err(|_| CryptoError::Aead)
}

/// Errors from the AEAD layer. Intentionally opaque: never leak plaintext or
/// distinguish failure causes to a caller that might log them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CryptoError {
    /// Invalid key length supplied to the cipher.
    Key,
    /// Seal/open failed (e.g. authentication tag mismatch on open).
    Aead,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::Key => write!(f, "invalid AES key length"),
            CryptoError::Aead => write!(f, "AES-GCM operation failed"),
        }
    }
}

impl std::error::Error for CryptoError {}

// ---------------------------------------------------------------------------
// v2 plaintext framing (data/iOS-app/v2/framing.md §1, §2, §3)
// ---------------------------------------------------------------------------

/// Framing version byte for `compress_payload` / `decompress_payload`
/// (framing.md §1). Always `0x01` today.
pub const FRAMING_VERSION: u8 = 0x01;
/// `comp` byte value: no compression (framing.md §2).
pub const COMP_NONE: u8 = 0x00;
/// `comp` byte value: zlib / DEFLATE (framing.md §2).
pub const COMP_ZLIB: u8 = 0x01;
/// Soglia: solo comprimere se `len(payload) > COMPRESS_THRESHOLD` (framing.md §2.3).
pub const COMPRESS_THRESHOLD: usize = 1024;
/// Hard ceiling on the decompressed payload size (defense-in-depth against a
/// zlib bomb from a compromised authorized device). A small ciphertext, under
/// the frame limit, could otherwise expand to many GB. 8 MiB is well above any
/// legitimate payload (the largest, `health_sync` on the live channel, is
/// bounded by `MAX_LIVE_CIPHERTEXT_BYTES` ≈ 512 KiB even before compression).
pub const MAX_DECOMPRESSED_BYTES: u64 = 8 * 1024 * 1024;

/// Compose the v2 plaintext frame around `payload`:
/// `version(0x01) ‖ comp(1B) ‖ payload`. Compresses with zlib if
/// `payload.len() > COMPRESS_THRESHOLD` (framing.md §2.3, §1).
pub fn compress_payload(payload: &[u8]) -> Vec<u8> {
    if payload.len() > COMPRESS_THRESHOLD {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(payload).expect("zlib write to Vec is infallible");
        let compressed = enc.finish().expect("zlib finish is infallible");
        let mut out = Vec::with_capacity(2 + compressed.len());
        out.push(FRAMING_VERSION);
        out.push(COMP_ZLIB);
        out.extend_from_slice(&compressed);
        out
    } else {
        let mut out = Vec::with_capacity(2 + payload.len());
        out.push(FRAMING_VERSION);
        out.push(COMP_NONE);
        out.extend_from_slice(payload);
        out
    }
}

/// Errors from the v2 plaintext framing (framing.md §3). The error variant
/// is intentionally opaque to avoid leaking information to upstream callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramingError {
    /// Truncated input (fewer than 2 bytes).
    Short,
    /// `version` byte is not `0x01`.
    BadVersion,
    /// `comp` byte is not in `{0x00, 0x01}`.
    BadComp,
    /// zlib decompress failed (corrupt compressed body) or the decompressed
    /// output exceeded [`MAX_DECOMPRESSED_BYTES`].
    Zlib,
}

impl std::fmt::Display for FramingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FramingError::Short => write!(f, "framing input shorter than 2 bytes"),
            FramingError::BadVersion => write!(f, "unknown framing version"),
            FramingError::BadComp => write!(f, "unknown compression algorithm"),
            FramingError::Zlib => write!(f, "zlib decompress failed"),
        }
    }
}
impl std::error::Error for FramingError {}

/// Decompose a v2 plaintext frame: return the original `payload` (i.e. the
/// bytes that were passed to `compress_payload`). Validates the framing
/// header and decompresses the body if `comp == 0x01` (framing.md §3).
pub fn decompress_payload(plaintext: &[u8]) -> Result<Vec<u8>, FramingError> {
    let (version, comp, body) = match plaintext.split_first_chunk::<2>() {
        Some((&[v, c], rest)) => (v, c, rest),
        None => return Err(FramingError::Short),
    };
    if version != FRAMING_VERSION {
        return Err(FramingError::BadVersion);
    }
    match comp {
        COMP_NONE => Ok(body.to_vec()),
        COMP_ZLIB => {
            // Cap the output: `take` makes the reader yield EOF at the limit, so
            // a zlib bomb stops there instead of exhausting memory. If the
            // decoder still has bytes left we hit exactly the limit and reject.
            let mut dec = ZlibDecoder::new(body).take(MAX_DECOMPRESSED_BYTES + 1);
            let mut out = Vec::with_capacity((body.len() * 2).min(MAX_DECOMPRESSED_BYTES as usize));
            dec.read_to_end(&mut out).map_err(|_| FramingError::Zlib)?;
            if out.len() as u64 > MAX_DECOMPRESSED_BYTES {
                return Err(FramingError::Zlib);
            }
            Ok(out)
        }
        _ => Err(FramingError::BadComp),
    }
}

// ---------------------------------------------------------------------------
// Pipe protocol crypto (docs/relay/pipe.md §3, §5). The data-plane auth reuses
// the Ed25519 challenge primitive with its own domain; the secure channel
// reuses ECDH + HKDF + AES-256-GCM, keyed by a *per-pipe ephemeral* DH (PFS).
// ---------------------------------------------------------------------------

/// Data-plane auth signing domain (pipe.md §2.1, `PIPE_AUTH_DOMAIN`).
pub const PIPE_AUTH_DOMAIN: &[u8] = b"skald-pipe-auth-v1";
/// HKDF salt for deriving the per-pipe AES key from the ephemeral ECDH secret.
pub const PIPE_KDF_SALT: &[u8] = b"skald-pipe-v1";
/// HKDF info for the per-pipe AES-256-GCM key.
pub const PIPE_KDF_INFO: &[u8] = b"pipe-aes-256-gcm";

/// Nonce direction prefix, pipe initiator → responder (pipe.md §3).
pub const DIR_PIPE_INITIATOR: [u8; 4] = [0, 0, 0, 3];
/// Nonce direction prefix, pipe responder → initiator (pipe.md §3).
pub const DIR_PIPE_RESPONDER: [u8; 4] = [0, 0, 0, 4];

/// Framing version byte marking a **pipe signaling** payload on the E2E channel:
/// `0x02 ‖ comp(0x00) ‖ msgpack`. Distinct from [`FRAMING_VERSION`] (`0x01`,
/// JSON app payloads) so the receiver can route by peeking the first byte
/// *without* changing [`decompress_payload`] (which still rejects `0x02`).
pub const FRAMING_VERSION_PIPE: u8 = 0x02;

/// Derive the per-pipe AES-256-GCM key from the ephemeral ECDH shared secret.
pub fn derive_pipe_key(eph_shared_secret: &[u8; 32]) -> [u8; 32] {
    hkdf32(eph_shared_secret, PIPE_KDF_SALT, PIPE_KDF_INFO)
}

/// Build the data-plane auth message that is signed/verified (pipe.md §2.1):
/// `PIPE_AUTH_DOMAIN ‖ 0x00 ‖ challenge_nonce(32B) ‖ connection_id(32B)`.
pub fn pipe_auth_message(challenge_nonce: &[u8; 32], connection_id: &[u8; 32]) -> Vec<u8> {
    let mut msg = Vec::with_capacity(PIPE_AUTH_DOMAIN.len() + 1 + 32 + 32);
    msg.extend_from_slice(PIPE_AUTH_DOMAIN);
    msg.push(0x00);
    msg.extend_from_slice(challenge_nonce);
    msg.extend_from_slice(connection_id);
    msg
}

/// Sign the data-plane auth message with `signing_key`. Returns the 64B signature.
pub fn sign_pipe_auth(
    signing_key: &SigningKey,
    challenge_nonce: &[u8; 32],
    connection_id: &[u8; 32],
) -> [u8; 64] {
    signing_key
        .sign(&pipe_auth_message(challenge_nonce, connection_id))
        .to_bytes()
}

/// Verify the data-plane auth signature under `ed25519_pub` (uses `verify_strict`).
pub fn verify_pipe_auth(
    ed25519_pub: &[u8; 32],
    challenge_nonce: &[u8; 32],
    connection_id: &[u8; 32],
    signature: &[u8; 64],
) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(ed25519_pub) else {
        return false;
    };
    let sig = Signature::from_bytes(signature);
    vk.verify_strict(&pipe_auth_message(challenge_nonce, connection_id), &sig)
        .is_ok()
}

/// Wrap a MsgPack pipe-signaling payload in the reserved framing:
/// `FRAMING_VERSION_PIPE ‖ COMP_NONE ‖ msgpack` (uncompressed; signaling is tiny).
pub fn frame_pipe_signal(msgpack: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + msgpack.len());
    out.push(FRAMING_VERSION_PIPE);
    out.push(COMP_NONE);
    out.extend_from_slice(msgpack);
    out
}

/// `true` if a decrypted E2E plaintext is a pipe-signaling frame (vs a `0x01`
/// app payload). A cheap first-byte peek used to route inbound messages.
pub fn is_pipe_signal(framed: &[u8]) -> bool {
    framed.first() == Some(&FRAMING_VERSION_PIPE)
}

/// Strip the pipe-signaling framing, returning the inner MsgPack body. `None`
/// if the header is not exactly `FRAMING_VERSION_PIPE ‖ COMP_NONE`.
pub fn unframe_pipe_signal(framed: &[u8]) -> Option<&[u8]> {
    match framed.split_first_chunk::<2>() {
        Some((&[FRAMING_VERSION_PIPE, COMP_NONE], body)) => Some(body),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn decode_hex_rejects_wrong_len() {
        assert!(decode_hex::<32>("00").is_none());
        assert!(decode_hex::<32>("zz".repeat(32).as_str()).is_none());
        assert_eq!(decode_hex::<2>("ABcd"), Some([0xAB, 0xCD])); // accepts uppercase
    }

    #[test]
    fn challenge_roundtrip() {
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        let nonce = [0x42u8; 32];

        let sig = sign_challenge(&sk, &nonce);
        assert!(verify_challenge(&pk, &nonce, &sig));

        // Different nonce → invalid signature.
        assert!(!verify_challenge(&pk, &[0x43u8; 32], &sig));
        // Different key → invalid.
        let other = SigningKey::from_bytes(&[8u8; 32])
            .verifying_key()
            .to_bytes();
        assert!(!verify_challenge(&other, &nonce, &sig));
    }

    #[test]
    fn namespace_id_is_hex_of_sha256() {
        let pk = [0u8; 32];
        let (raw, hexed) = namespace_id(&pk);
        assert_eq!(hexed.len(), 64);
        assert_eq!(hex::encode(raw), hexed);
    }

    #[test]
    fn ecdh_is_symmetric_and_seals_roundtrip() {
        let seed_a: [u8; 32] = (0u8..32).collect::<Vec<_>>().try_into().unwrap();
        let seed_c: [u8; 32] = (32u8..64).collect::<Vec<_>>().try_into().unwrap();
        let a = derive_keys(&seed_a);
        let c = derive_keys(&seed_c);

        let s1 = ecdh(&a.x25519_priv, &c.x25519_pub);
        let s2 = ecdh(&c.x25519_priv, &a.x25519_pub);
        assert_eq!(s1, s2, "ECDH must be symmetric");

        let key = derive_aes_key(&s1);
        let (ns_raw, _) = namespace_id(&a.ed25519_pub);
        let nonce = build_nonce(DIR_AGENT_TO_CLIENT, 1);
        let aad = build_aad(&ns_raw, &a.ed25519_pub, &c.ed25519_pub);

        let pt = b"hello world";
        let sealed = seal(&key, &nonce, &aad, pt).unwrap();
        let opened = open(&key, &nonce, &aad, &sealed).unwrap();
        assert_eq!(opened, pt);

        // Tampered AAD must fail to open.
        let mut bad_aad = aad.clone();
        bad_aad[0] ^= 1;
        assert!(open(&key, &nonce, &bad_aad, &sealed).is_err());
    }

    /// Cross-compat: the iOS client uses `CryptoKit` to sign the auth challenge,
    /// the relay uses `ed25519-dalek` to verify.  The two libraries produce
    /// *different* (but both valid) Ed25519 signatures for the same (key,
    /// message) — see `data/ios-app/test-vectors.md` §4 "Note on V18".  This test
    /// pins the two signatures the relay must accept for the canonical
    /// `SEED_CLIENT = bytes(32..<64)` test vector.
    #[test]
    fn challenge_verifies_cryptokit_signature() {
        // V8 client_ed25519_pub from test-vectors.md §4.
        let client_pub: [u8; 32] =
            hex::decode("12355ea750e60d6370ba6776037f25062f6c9450c5009669884895fd5b377a18")
                .unwrap()
                .try_into()
                .unwrap();
        let challenge: [u8; 32] =
            hex::decode("aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899")
                .unwrap()
                .try_into()
                .unwrap();
        // V18 from test-vectors.md §4 (CryptoKit signature).
        let v18_cryptokit: [u8; 64] =
            hex::decode("a2af4518a7001e3269006d30e1175d33d36cc23350c6c4def8347be8ec97e32ce51c4f066ca29cc497690aa241524d20ea20a72d38d9beb6da01e966aada8508")
                .unwrap()
                .try_into()
                .unwrap();
        // The dalek reference value (regression — see "Note on V18" in §4).
        let v18_dalek: [u8; 64] =
            hex::decode("ae38491a1f25bb5fb11f0b17e3d344412bfc927461b6517e9a0ab6a64020054677f59490af026f34c81d9378d4daae4823109ca2d1afbf4ff00230a038270002")
                .unwrap()
                .try_into()
                .unwrap();

        assert!(verify_challenge(&client_pub, &challenge, &v18_cryptokit),
                "relay must accept the CryptoKit signature committed as V18");
        assert!(verify_challenge(&client_pub, &challenge, &v18_dalek),
                "relay must also accept the historical dalek reference signature");

        // Sanity: tampered signature is rejected.
        let mut bad = v18_cryptokit;
        bad[0] ^= 0x01;
        assert!(!verify_challenge(&client_pub, &challenge, &bad));
    }

    /// Pin the derived keys / namespace / aes_key against the committed vectors
    /// (test-vectors.md §4) so the library functions can never silently diverge.
    #[test]
    fn library_matches_reference_vectors() {
        let seed_a: [u8; 32] = (0u8..32).collect::<Vec<_>>().try_into().unwrap();
        let a = derive_keys(&seed_a);
        assert_eq!(
            hex::encode(a.ed25519_pub),
            "b3e202f4ac99fd9929da47df20adedd5b2598411a466a229f086eda3467ffa7b"
        );
        let (ns_raw, ns_hex) = namespace_id(&a.ed25519_pub);
        assert_eq!(
            ns_hex,
            "f7d340d3c3f0b0052fa904ba60ebd38a0f7e7d10672ac80648991a2c632c9e58"
        );

        let seed_c: [u8; 32] = (32u8..64).collect::<Vec<_>>().try_into().unwrap();
        let c = derive_keys(&seed_c);
        let shared = ecdh(&a.x25519_priv, &c.x25519_pub);
        assert_eq!(
            hex::encode(shared),
            "66c51034dd6360b9cdddc495049463b0191d7f3bddce9ea6f2975c85d471540a"
        );
        let aes_key = derive_aes_key(&shared);
        assert_eq!(
            hex::encode(aes_key),
            "74fb4ffcbbe069859cfb0790023811554dad328d9f4ac4a1d28077086e33a4e7"
        );
        let _ = ns_raw;
    }

    #[test]
    fn framing_round_trip_small_payload_no_compression() {
        // Below threshold → comp = 0x00
        let payload = b"{\"v\":1,\"kind\":\"x\"}";
        let framed = compress_payload(payload);
        assert_eq!(framed[0], FRAMING_VERSION);
        assert_eq!(framed[1], COMP_NONE);
        assert_eq!(&framed[2..], payload);
        assert_eq!(decompress_payload(&framed).unwrap(), payload);
    }

    #[test]
    fn framing_round_trip_large_payload_zlib() {
        // Build a payload that clearly crosses the threshold and is
        // highly compressible (a repeated string → ~1% of original).
        let payload: Vec<u8> = "skald ".repeat(500).into_bytes();
        assert!(payload.len() > COMPRESS_THRESHOLD);
        let framed = compress_payload(&payload);
        assert_eq!(framed[0], FRAMING_VERSION);
        assert_eq!(framed[1], COMP_ZLIB);
        // Compressed body must be smaller than the original.
        assert!(framed.len() < payload.len());
        assert_eq!(decompress_payload(&framed).unwrap(), payload);
    }

    #[test]
    fn framing_rejects_bad_version() {
        let mut bad = vec![0x02, COMP_NONE];
        bad.extend_from_slice(b"x");
        assert_eq!(decompress_payload(&bad), Err(FramingError::BadVersion));
    }

    #[test]
    fn framing_rejects_bad_comp() {
        let mut bad = vec![FRAMING_VERSION, 0x02];
        bad.extend_from_slice(b"x");
        assert_eq!(decompress_payload(&bad), Err(FramingError::BadComp));
    }

    #[test]
    fn framing_rejects_truncated_input() {
        assert_eq!(decompress_payload(&[]), Err(FramingError::Short));
        assert_eq!(
            decompress_payload(&[FRAMING_VERSION]),
            Err(FramingError::Short)
        );
    }

    #[test]
    fn framing_rejects_corrupt_zlib_body() {
        let mut bad = vec![FRAMING_VERSION, COMP_ZLIB];
        bad.extend_from_slice(b"this is not zlib data");
        assert_eq!(decompress_payload(&bad), Err(FramingError::Zlib));
    }

    #[test]
    fn framing_rejects_zlib_bomb_over_limit() {
        // A tiny compressed frame that decompresses past MAX_DECOMPRESSED_BYTES
        // must be rejected, not allocated. Zeros compress to a handful of bytes.
        let huge = vec![0u8; (MAX_DECOMPRESSED_BYTES as usize) + 1];
        let framed = compress_payload(&huge);
        assert!(framed.len() < huge.len() / 100, "bomb frame compresses hugely");
        assert_eq!(decompress_payload(&framed), Err(FramingError::Zlib));
    }

    #[test]
    fn framing_accepts_payload_at_limit() {
        // Exactly at the ceiling must still round-trip.
        let big = vec![7u8; MAX_DECOMPRESSED_BYTES as usize];
        let framed = compress_payload(&big);
        assert_eq!(decompress_payload(&framed).unwrap(), big);
    }

    #[test]
    fn pipe_auth_sign_verify_binds_nonce_and_connection() {
        let sk = SigningKey::from_bytes(&[9u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        let nonce = [0x11u8; 32];
        let cid = [0x22u8; 32];

        let sig = sign_pipe_auth(&sk, &nonce, &cid);
        assert!(verify_pipe_auth(&pk, &nonce, &cid, &sig));
        // Different nonce or connection_id → invalid (anti-replay / rebind).
        assert!(!verify_pipe_auth(&pk, &[0x12u8; 32], &cid, &sig));
        assert!(!verify_pipe_auth(&pk, &nonce, &[0x23u8; 32], &sig));
        // Domain separation: an AUTH_DOMAIN signature must not verify here.
        let auth_sig = sign_challenge(&sk, &nonce);
        assert!(!verify_pipe_auth(&pk, &nonce, &cid, &auth_sig));
    }

    #[test]
    fn pipe_key_is_symmetric_over_ephemeral_dh() {
        // Two fresh ephemeral X25519 keypairs derive the same pipe key.
        let a = derive_keys(&[1u8; 32]);
        let b = derive_keys(&[2u8; 32]);
        let ka = derive_pipe_key(&ecdh(&a.x25519_priv, &b.x25519_pub));
        let kb = derive_pipe_key(&ecdh(&b.x25519_priv, &a.x25519_pub));
        assert_eq!(ka, kb);
    }

    #[test]
    fn pipe_signal_framing_round_trip() {
        let body = b"\x82\xa1k\x01"; // arbitrary msgpack-ish bytes
        let framed = frame_pipe_signal(body);
        assert!(is_pipe_signal(&framed));
        assert_eq!(unframe_pipe_signal(&framed), Some(&body[..]));
        // A normal v2 app frame is NOT a pipe signal and won't unframe.
        let app = compress_payload(b"{}");
        assert!(!is_pipe_signal(&app));
        assert_eq!(unframe_pipe_signal(&app), None);
    }
}
