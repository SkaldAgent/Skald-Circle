//! Envelope encryption for the per-user databases (blueprint §4 / §5.1).
//!
//! A user's database is encrypted by SQLCipher under a random 256-bit **DEK**.
//! The DEK is never stored in the clear: `users.database_password` holds it
//! sealed with AES-256-GCM under a **KEK** derived from the password with
//! Argon2id.
//!
//! That seal *is* the password verifier. Opening it either yields the DEK — in
//! which case the password was right — or fails the AEAD tag, which means a
//! wrong password, cleanly distinct from a corrupt file. Storing a second hash
//! of the same password beside the seal would only hand an offline attacker an
//! easier target than the seal itself, so encrypted users have no
//! `password_hash` at all.
//!
//! Cleartext users have no database key to bind a verifier to, so they store the
//! Argon2id output directly and it is compared in constant time. Its
//! crackability is harmless: their database is readable by the box owner by
//! design.
//!
//! Nothing here invents a construction — it composes Argon2id, AES-256-GCM and a
//! CSPRNG. Changing the password re-seals the same DEK; the database itself is
//! never re-encrypted.

use std::fmt::{self, Write as _};
use std::sync::LazyLock;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use argon2::{Algorithm, Argon2, Params, Version};
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use zeroize::{Zeroize, ZeroizeOnDrop};

/// 256-bit keys throughout: AES-256-GCM for the seal, raw SQLCipher key for the
/// database.
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
/// Sealed layout: `nonce ‖ ciphertext ‖ tag`.
const SEALED_LEN: usize = NONCE_LEN + KEY_LEN + TAG_LEN;

pub const SALT_LEN: usize = 16;

/// The only algorithm we accept. Stored per row so it can change later without a
/// migration, but a row asking for anything else is a bug, not a fallback.
const ALGO: &str = "argon2id";

/// Argon2id at 256 MiB is a memory bomb if it runs unbounded: every concurrent
/// login would allocate its own arena. Two at a time keeps a login responsive
/// while capping the peak at ~2× `KdfParams::m`.
///
/// Deliberately a module-level invariant rather than a caller's responsibility —
/// a forgotten permit is a memory incident, not a style problem.
static KDF_GATE: LazyLock<Semaphore> = LazyLock::new(|| Semaphore::new(2));

// ── KDF parameters ────────────────────────────────────────────────────────────

/// Serialized into `users.kdf_params`. Not secret: calibrated on the box when the
/// user is created, and kept per row so raising the cost later needs no migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub algo: String,
    /// Memory cost, in KiB.
    pub m: u32,
    /// Time cost (passes).
    pub t: u32,
    /// Parallelism (lanes).
    pub p: u32,
}

impl Default for KdfParams {
    /// 256 MiB, ~1s on the weakest box we target.
    ///
    /// Between the OWASP baseline (64 MiB) and the 512 MiB–1 GiB of §5.1: four
    /// times the cost for an attacker with a GPU, while two concurrent logins
    /// peak at 512 MiB rather than 2 GiB and never reach swap.
    fn default() -> Self {
        Self { algo: ALGO.to_string(), m: 262_144, t: 3, p: 1 }
    }
}

impl KdfParams {
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serializing kdf_params")
    }

    pub fn from_json(s: &str) -> Result<Self> {
        let p: Self = serde_json::from_str(s).context("parsing kdf_params")?;
        if p.algo != ALGO {
            bail!("unsupported kdf algorithm: {}", p.algo);
        }
        Ok(p)
    }

    /// Cheap parameters, tests only: the real ones cost ~1s and 256 MiB per
    /// derivation, which a test suite pays on every register and every login.
    #[cfg(test)]
    pub fn fast() -> Self {
        Self { algo: ALGO.to_string(), m: 64, t: 1, p: 1 }
    }
}

// ── Keys ──────────────────────────────────────────────────────────────────────

/// Data Encryption Key: the raw SQLCipher key for one user's database. Random,
/// never derived from the password, so changing the password does not re-encrypt
/// anything.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Dek([u8; KEY_LEN]);

/// Key Encryption Key: `Argon2id(password, salt)`. Seals the [`Dek`], and for a
/// cleartext user its raw bytes *are* the stored verifier.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Kek([u8; KEY_LEN]);

impl Dek {
    pub fn random() -> Self {
        let mut k = [0u8; KEY_LEN];
        rand::rng().fill_bytes(&mut k);
        Self(k)
    }

    /// The `PRAGMA key` value for a raw 256-bit key.
    ///
    /// The double quotes are **part of the value**: sqlx pastes it verbatim into
    /// `PRAGMA key = {value};`, and SQLCipher only treats the argument as a raw
    /// key when it parses as the blob literal `x'…'`. Given anything else it
    /// runs its own KDF over the bytes instead, silently deriving a *different*
    /// key from our hex digits — and the database would open, just not the one
    /// we meant.
    ///
    /// The returned string is key material. It must never be logged, and it
    /// outlives this call inside the pool's connect options.
    pub fn to_pragma(&self) -> String {
        // `"x'` + 64 hex digits + `'"`
        let mut s = String::with_capacity(5 + 2 * KEY_LEN);
        s.push_str("\"x'");
        for b in self.0 {
            let _ = write!(s, "{b:02x}");
        }
        s.push_str("'\"");
        s
    }
}

impl Kek {
    /// The verifier stored in `users.password_hash` for a cleartext user.
    pub fn as_verifier(&self) -> &[u8] {
        &self.0
    }
}

// Hand-written so a stray `{:?}` — a tracing span, an error context, a panic —
// cannot print key material. Same reasoning as `db::users::Credentials`.
impl fmt::Debug for Dek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Dek(<redacted>)")
    }
}

impl fmt::Debug for Kek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Kek(<redacted>)")
    }
}

pub fn random_salt() -> Vec<u8> {
    let mut s = vec![0u8; SALT_LEN];
    rand::rng().fill_bytes(&mut s);
    s
}

// ── Derivation ────────────────────────────────────────────────────────────────

/// `Argon2id(password, salt)` → 256-bit key.
///
/// Runs on a blocking thread — it burns a core and 256 MiB for about a second,
/// which would stall a tokio worker — and behind [`KDF_GATE`].
pub async fn derive_kek(password: &str, salt: &[u8], params: &KdfParams) -> Result<Kek> {
    let _permit = KDF_GATE.acquire().await.context("kdf gate closed")?;

    let mut password = password.to_owned();
    let salt = salt.to_vec();
    let params = params.clone();

    let out = tokio::task::spawn_blocking(move || {
        let result = derive_blocking(password.as_bytes(), &salt, &params);
        password.zeroize();
        result
    })
    .await
    .context("argon2 task panicked")?;

    out
}

fn derive_blocking(password: &[u8], salt: &[u8], params: &KdfParams) -> Result<Kek> {
    if params.algo != ALGO {
        bail!("unsupported kdf algorithm: {}", params.algo);
    }
    let p = Params::new(params.m, params.t, params.p, Some(KEY_LEN))
        .map_err(|e| anyhow!("invalid argon2 params: {e}"))?;
    let mut key = [0u8; KEY_LEN];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, p)
        .hash_password_into(password, salt, &mut key)
        .map_err(|e| anyhow!("argon2 derivation failed: {e}"))?;
    Ok(Kek(key))
}

/// Constant-time comparison of a freshly derived key against a stored verifier.
/// `subtle` returns "not equal" for a length mismatch rather than short-circuiting.
pub fn verify(derived: &Kek, stored: &[u8]) -> bool {
    derived.0.ct_eq(stored).into()
}

// ── Envelope ──────────────────────────────────────────────────────────────────

/// Why a seal did not open. The distinction is the whole point: a failed tag is
/// an authentication answer, a malformed blob is a broken row.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyError {
    /// The AEAD tag did not verify. This *is* the wrong-password signal.
    WrongPassword,
    Malformed(&'static str),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::WrongPassword => f.write_str("wrong password"),
            KeyError::Malformed(w) => write!(f, "malformed wrapped key: {w}"),
        }
    }
}

impl std::error::Error for KeyError {}

/// Seals `dek` under `kek`. Output is `nonce ‖ ciphertext ‖ tag`, stored as-is in
/// `users.database_password`.
pub fn wrap_dek(kek: &Kek, dek: &Dek) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(&kek.0).map_err(|e| anyhow!("bad kek length: {e}"))?;

    let mut nonce = [0u8; NONCE_LEN];
    rand::rng().fill_bytes(&mut nonce);

    let sealed = cipher
        .encrypt(Nonce::from_slice(&nonce), dek.0.as_slice())
        .map_err(|_| anyhow!("AEAD seal failed"))?;

    let mut out = Vec::with_capacity(SEALED_LEN);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Opens a seal produced by [`wrap_dek`]. A failed tag returns
/// [`KeyError::WrongPassword`] — one Argon2id pass answers "is the password
/// right" *and* hands back the key.
pub fn unwrap_dek(kek: &Kek, sealed: &[u8]) -> Result<Dek, KeyError> {
    if sealed.len() != SEALED_LEN {
        return Err(KeyError::Malformed("unexpected length"));
    }
    let (nonce, body) = sealed.split_at(NONCE_LEN);

    let cipher =
        Aes256Gcm::new_from_slice(&kek.0).map_err(|_| KeyError::Malformed("bad kek length"))?;

    let mut plain = cipher
        .decrypt(Nonce::from_slice(nonce), body)
        .map_err(|_| KeyError::WrongPassword)?;

    if plain.len() != KEY_LEN {
        plain.zeroize();
        return Err(KeyError::Malformed("unexpected plaintext length"));
    }

    let mut dek = [0u8; KEY_LEN];
    dek.copy_from_slice(&plain);
    plain.zeroize();
    Ok(Dek(dek))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn kek(password: &str, salt: &[u8]) -> Kek {
        derive_kek(password, salt, &KdfParams::fast()).await.unwrap()
    }

    #[tokio::test]
    async fn seal_round_trips_and_the_tag_is_the_password_check() {
        let salt = random_salt();
        let dek = Dek::random();
        let sealed = wrap_dek(&kek("correct horse", &salt).await, &dek).unwrap();
        assert_eq!(sealed.len(), SEALED_LEN);

        let opened = unwrap_dek(&kek("correct horse", &salt).await, &sealed).unwrap();
        assert_eq!(opened.0, dek.0, "the same DEK must come back out");

        // The whole auth story: a wrong password is a failed AEAD tag, and it is
        // reported as such — not as a corrupt blob.
        let err = unwrap_dek(&kek("wrong horse", &salt).await, &sealed).unwrap_err();
        assert_eq!(err, KeyError::WrongPassword);
    }

    #[tokio::test]
    async fn a_truncated_seal_is_malformed_not_a_wrong_password() {
        let salt = random_salt();
        let sealed = wrap_dek(&kek("pw", &salt).await, &Dek::random()).unwrap();
        let err = unwrap_dek(&kek("pw", &salt).await, &sealed[..SEALED_LEN - 1]).unwrap_err();
        assert!(matches!(err, KeyError::Malformed(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn the_salt_separates_identical_passwords() {
        let a = kek("same", &random_salt()).await;
        let b = kek("same", &random_salt()).await;
        assert_ne!(a.0, b.0, "a per-user salt must defuse rainbow tables");
    }

    #[tokio::test]
    async fn verify_accepts_the_right_password_and_rejects_a_short_verifier() {
        let salt = random_salt();
        let stored = kek("pw", &salt).await;
        assert!(verify(&kek("pw", &salt).await, stored.as_verifier()));
        assert!(!verify(&kek("nope", &salt).await, stored.as_verifier()));
        // A length mismatch must not panic, and must not compare equal.
        assert!(!verify(&kek("pw", &salt).await, &stored.as_verifier()[..16]));
    }

    #[test]
    fn pragma_is_a_quoted_blob_literal() {
        let dek = Dek([0xAB; KEY_LEN]);
        let p = dek.to_pragma();
        assert!(p.starts_with("\"x'") && p.ends_with("'\""), "got {p}");
        assert_eq!(p.len(), 5 + 2 * KEY_LEN, "\"x' + 64 hex digits + '\"");
        assert!(p.contains("abab"), "lowercase hex");
    }

    #[test]
    fn debug_never_prints_key_material() {
        let dek = Dek([0xDE; KEY_LEN]);
        let kek = Kek([0xAD; KEY_LEN]);
        assert_eq!(format!("{dek:?}"), "Dek(<redacted>)");
        assert_eq!(format!("{kek:?}"), "Kek(<redacted>)");
        assert!(!format!("{dek:?}").contains("dede"), "no raw key bytes");
        assert!(!format!("{kek:?}").contains("adad"), "no raw key bytes");
    }

    #[test]
    fn kdf_params_round_trip_and_reject_foreign_algorithms() {
        let p = KdfParams::default();
        assert_eq!(p.m, 262_144);
        assert_eq!(KdfParams::from_json(&p.to_json().unwrap()).unwrap(), p);

        let pbkdf2 = r#"{"algo":"pbkdf2","m":1,"t":1,"p":1}"#;
        assert!(KdfParams::from_json(pbkdf2).is_err(), "only argon2id is accepted");
    }
}
