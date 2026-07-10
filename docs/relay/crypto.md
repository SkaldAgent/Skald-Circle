# Crypto Contract

> This file is the **single source of truth** for cryptography. Relay, plugin, and app MUST
> implement exactly what follows. Any divergence breaks interoperability. The words MUST / MUST NOT /
> SHOULD carry the RFC 2119 meaning. Verify your implementation against
> [test-vectors.md](test-vectors.md) **before** integrating.

Field encoding: see [index.md §5](index.md). In short: keys/signatures/ids/nonces in **lowercase
hex**, ciphertext in **standard base64 with padding**.

---

## 1. Domain Constants (NORMATIVE)

All strings are ASCII/UTF-8, without NUL terminator unless noted as `\x00`.

| Name | Value (bytes) | Use |
|------|---------------|-----|
| `KDF_SALT` | `"skald-kdf-v1"` | HKDF seed → keypair (§3) |
| `KDF_INFO_X25519` | `"x25519"` | HKDF info, X25519 branch (§3) |
| `KDF_INFO_ED25519` | `"ed25519"` | HKDF info, ed25519 branch (§3) |
| `SESSION_SALT` | `"skald-session-v1"` | HKDF shared_secret → aes_key (§5) |
| `SESSION_INFO` | `"aes-256-gcm"` | HKDF info, AEAD key (§5) |
| `NS_DOMAIN` | `"skald-namespace-v1"` | `namespace_id` derivation (§7) |
| `AUTH_DOMAIN` | `"skald-relay-auth-v1"` | Challenge-response signature (§8) |
| `NONCE_DIR_AGENT_TO_CLIENT` | `0x00 0x00 0x00 0x01` | Nonce prefix, agent→client direction (§6) |
| `NONCE_DIR_CLIENT_TO_AGENT` | `0x00 0x00 0x00 0x02` | Nonce prefix, client→agent direction (§6) |
| `PIPE_AUTH_DOMAIN` | `"skald-pipe-auth-v1"` | Pipe data-plane challenge signature ([pipe.md §3.1](pipe.md)) |
| `PIPE_KDF_SALT` | `"skald-pipe-v1"` | HKDF salt: ephemeral ECDH → per-pipe AES key ([pipe.md §4](pipe.md)) |
| `PIPE_KDF_INFO` | `"pipe-aes-256-gcm"` | HKDF info, per-pipe AES key ([pipe.md §4](pipe.md)) |
| `NONCE_DIR_PIPE_INITIATOR` | `0x00 0x00 0x00 0x03` | Nonce prefix, pipe initiator→responder ([pipe.md §4](pipe.md)) |
| `NONCE_DIR_PIPE_RESPONDER` | `0x00 0x00 0x00 0x04` | Nonce prefix, pipe responder→initiator ([pipe.md §4](pipe.md)) |

Algorithms: **X25519** (RFC 7748), **Ed25519** (RFC 8032), **HKDF-SHA256** (RFC 5869),
**AES-256-GCM** (NIST SP 800-38D), **SHA-256** (FIPS 180-4).

> The **pipe** (relayed byte-stream, [pipe.md](pipe.md)) reuses this entire suite — X25519 ECDH,
> HKDF, AES-256-GCM with the `DIR ‖ counter` nonce (§6) — keyed by a **per-pipe ephemeral** DH
> (Perfect Forward Secrecy), with `aad = connection_id`. No new primitives.

---

## 2. Persistent Material: the Seed

Every actor with a cryptographic identity (agent and each client) holds **one single persistent
secret**: a **32-byte seed** generated from CSPRNG.

- Agent: `data/relay/seed`, 32-byte binary file, permissions `0600`. Generated on first start.
- iOS client: 32 bytes in Keychain, attribute `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`.
- Android client: 32 bytes in Keystore / EncryptedSharedPreferences (hardware-backed if available).

Two keypairs are derived from this seed (§3). The seed MUST NOT leave the device and MUST NOT
ever be transmitted. Private keys are regenerated from the seed on each startup; they are not
persisted separately.

> **Why two keypairs?** Ed25519 is for **signing** (authentication toward the relay).
> X25519 is for **ECDH** (E2E key agreement). They are related curves with distinct roles and APIs
> on all platforms (CryptoKit separates them: `Curve25519.Signing` vs `Curve25519.KeyAgreement`).
> **Never** convert an ed25519 key into X25519 by reinterpreting the bytes: this is cryptographically
> wrong. Both are derived independently from the seed.

---

## 3. Key Derivation from Seed (NORMATIVE)

Identical across all platforms. `HKDF` = HKDF-SHA256, 32-byte output.

```
x25519_priv  = HKDF(ikm = seed, salt = KDF_SALT, info = KDF_INFO_X25519, len = 32)
ed25519_priv = HKDF(ikm = seed, salt = KDF_SALT, info = KDF_INFO_ED25519, len = 32)
```

- `x25519_priv` (32 bytes) is the X25519 private **scalar**. Libraries apply RFC 7748 *clamping*
  internally; do not clamp manually. `x25519_pub = X25519(x25519_priv, basepoint)`.
- `ed25519_priv` (32 bytes) is the **Ed25519 seed** (the 32-byte "private key" of RFC 8032).
  `ed25519_pub` (32 bytes) is derived from it per RFC 8032.

> Terminology note: in Ed25519, the 64-byte "private key" is `seed(32) ‖ pub(32)`. Here the secret
> material is the **32-byte seed** (`ed25519_priv` above). Do not confuse the 32 bytes of *our* seed
> (§2) with the 32 bytes of the *Ed25519 seed* (HKDF output): they are different things.

### Rust (agent / relay-side verification)

```rust
use hkdf::Hkdf;
use sha2::Sha256;
use ed25519_dalek::SigningKey;                  // ed25519-dalek = "2"
use x25519_dalek::{StaticSecret, PublicKey};    // x25519-dalek   = "2"

fn derive_keys(seed: &[u8; 32]) -> (SigningKey, StaticSecret) {
    let hk = Hkdf::<Sha256>::new(Some(b"skald-kdf-v1"), seed);

    let mut x = [0u8; 32];
    hk.expand(b"x25519", &mut x).unwrap();
    let x25519_priv = StaticSecret::from(x);    // internal clamping

    let mut e = [0u8; 32];
    hk.expand(b"ed25519", &mut e).unwrap();
    let ed25519_priv = SigningKey::from_bytes(&e);

    (ed25519_priv, x25519_priv)
}
// pub keys:
//   ed25519_pub = signing_key.verifying_key().to_bytes()    // 32B
//   x25519_pub  = PublicKey::from(&x25519_priv).to_bytes()  // 32B
```

### Swift (iOS, CryptoKit)

```swift
import CryptoKit

func deriveKeys(seed: Data) -> (signing: Curve25519.Signing.PrivateKey,
                                agreement: Curve25519.KeyAgreement.PrivateKey) {
    let ikm = SymmetricKey(data: seed)
    let salt = Data("skald-kdf-v1".utf8)

    let xRaw = HKDF<SHA256>.deriveKey(inputKeyMaterial: ikm, salt: salt,
                 info: Data("x25519".utf8), outputByteCount: 32)
    let eRaw = HKDF<SHA256>.deriveKey(inputKeyMaterial: ikm, salt: salt,
                 info: Data("ed25519".utf8), outputByteCount: 32)

    let agreement = try! Curve25519.KeyAgreement.PrivateKey(
                        rawRepresentation: xRaw.withUnsafeBytes { Data($0) })
    let signing   = try! Curve25519.Signing.PrivateKey(
                        rawRepresentation: eRaw.withUnsafeBytes { Data($0) })
    return (signing, agreement)
}
```

### Kotlin (Android — reference)

Use **BouncyCastle / Tink**: `HKDFBytesGenerator(SHA256Digest)` with the same salt/info, then
`X25519PrivateKeyParameters` and `Ed25519PrivateKeyParameters` from the 32 derived bytes.

---

## 4. ECDH — Key Agreement (X25519, ONLY path)

The agent and each client exchange their **X25519 public key** (the agent via QR; the client via
the pairing frame — see [relay-protocol.md](relay-protocol.md)). The shared secret:

```
shared_secret = X25519(my_x25519_priv, peer_x25519_pub)      // 32 bytes
```

It is symmetric: `X25519(a_priv, b_pub) == X25519(b_priv, a_pub)`. **MUST** always and only use
X25519 keys. Ed25519 keys NEVER enter ECDH.

```rust
let shared = my_x25519_priv.diffie_hellman(&PublicKey::from(peer_x25519_pub_bytes));
let shared_secret: [u8; 32] = *shared.as_bytes();
```

```swift
let peerPub = try Curve25519.KeyAgreement.PublicKey(rawRepresentation: peerX25519PubBytes)
let shared  = try myAgreementPriv.sharedSecretFromKeyAgreement(with: peerPub)
// `shared` is a SharedSecret; do NOT use it raw: pass through HKDF (§5).
```

> **Point validation.** Standard libraries (x25519-dalek, CryptoKit) handle low-order points;
> an implementation that does not MUST reject an all-zero shared secret.

---

## 5. AEAD Key Derivation (HKDF)

The raw shared secret is never used directly as a key. It is derived:

```
aes_key = HKDF(ikm = shared_secret, salt = SESSION_SALT, info = SESSION_INFO, len = 32)
```

```rust
let hk = Hkdf::<Sha256>::new(Some(b"skald-session-v1"), &shared_secret);
let mut aes_key = [0u8; 32];
hk.expand(b"aes-256-gcm", &mut aes_key).unwrap();
```

```swift
let aesKey = shared.hkdfDerivedSymmetricKey(using: SHA256.self,
                salt: Data("skald-session-v1".utf8),
                sharedInfo: Data("aes-256-gcm".utf8),
                outputByteCount: 32)
```

`aes_key` is **per-peer** (one per agent↔client pair) and static for the life of the pairing
(no PFS in the current protocol).

---

## 6. AEAD — AES-256-GCM with Counter Nonce and AAD (NORMATIVE)

**All** E2E messages are encrypted this way. There is no separate MAC: **GCM is already
authenticated**. (No separate HMAC — it would be redundant and violate key-separation.)

### 6.1 Nonce — Monotonic Counter, NOT Random

The GCM nonce is **12 bytes** and is built deterministically to prevent reuse and provide
**anti-replay**:

```
nonce (12B) = DIR (4B) ‖ counter (8B, big-endian)
```

- `DIR` = `NONCE_DIR_AGENT_TO_CLIENT` if the encryptor is the agent, `NONCE_DIR_CLIENT_TO_AGENT`
  if it is the client. Ensures the two directions never collide even though they share `aes_key`.
- `counter` is a **strictly increasing** 64-bit integer, **persisted per-peer and per-direction**.
  Starts at `1`. Increments by 1 per sent message. MUST be persisted **before** sending (so a
  crash cannot cause reuse).

The **receiver** maintains `last_seen_counter` for (peer, direction) and MUST reject any message
with `counter <= last_seen_counter` (replay or reorder). Under FIFO store-and-forward delivery,
counters arrive in order; a forward gap is allowed (messages lost), a value `<=` is not.

> Consequence: counters are the primary **anti-replay state**. They survive reconnections and
> restarts because they are persisted. If the send counter is irreversibly reset (e.g. seed
> restored without state), a **re-pairing** is required (new `aes_key`, counters reset together).

### 6.2 AAD — Routing Binding

The AAD (Additional Authenticated Data) binds the ciphertext to routing metadata, so a malicious
relay relabelling `from`/`to` causes decryption to **fail**:

```
AAD (96B) = namespace_id_raw (32B) ‖ from_pubkey (32B) ‖ to_pubkey (32B)
```

- `namespace_id_raw` = the 32 raw bytes of the hash from §7 (NOT the hex string).
- `from_pubkey`, `to_pubkey` = **ed25519** public keys (32 raw bytes) of sender and recipient
  (same values used for routing in the envelope).
- The receiver reconstructs the AAD from the `from`/`to` fields of the received envelope and its
  own `namespace_id`. If they do not match those used in encryption → invalid GCM tag → discard.

### 6.3 Encrypted Block Format

```
sealed = ciphertext ‖ tag(16B)          // GCM "combined" output, WITHOUT nonce
```

The `nonce` travels **in plaintext** in a separate envelope field (it is public by definition;
its integrity is guaranteed because GCM uses it as an authenticated IV). On the wire (inside the
E2E JSON payload, before the framing of §… is applied):

```json
{ "nonce": "<hex 24>", "ciphertext": "<base64 of (ciphertext‖tag)>" }
```

In the protobuf transport (`Message` frame) the fields are raw bytes — no hex, no base64.

### 6.4 Rust

```rust
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use aes_gcm::aead::{Aead, Payload};

fn seal(aes_key: &[u8;32], dir: [u8;4], counter: u64,
        aad: &[u8;96], plaintext: &[u8]) -> (Vec<u8> /*nonce*/, Vec<u8> /*sealed*/) {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&dir);
    nonce[4..].copy_from_slice(&counter.to_be_bytes());

    let cipher = Aes256Gcm::new(aes_key.into());
    let sealed = cipher.encrypt(Nonce::from_slice(&nonce),
                    Payload { msg: plaintext, aad }).expect("encrypt");
    (nonce.to_vec(), sealed)
}

fn open(aes_key: &[u8;32], nonce: &[u8;12], aad: &[u8;96], sealed: &[u8]) -> Option<Vec<u8>> {
    let cipher = Aes256Gcm::new(aes_key.into());
    cipher.decrypt(Nonce::from_slice(nonce), Payload { msg: sealed, aad }).ok()
}
```

### 6.5 Swift

```swift
func seal(aesKey: SymmetricKey, dir: [UInt8], counter: UInt64,
          aad: Data, plaintext: Data) throws -> (nonce: Data, sealed: Data) {
    var n = Data(dir)                                   // 4B
    var be = counter.bigEndian
    n.append(Data(bytes: &be, count: 8))                // +8B = 12B
    let nonce = try AES.GCM.Nonce(data: n)
    let box = try AES.GCM.seal(plaintext, using: aesKey,
                               nonce: nonce, authenticating: aad)
    // box.ciphertext ‖ box.tag  == "sealed"
    return (n, box.ciphertext + box.tag)
}

func open(aesKey: SymmetricKey, nonce: Data, aad: Data, sealed: Data) throws -> Data {
    let ct = sealed.prefix(sealed.count - 16)
    let tag = sealed.suffix(16)
    let box = try AES.GCM.SealedBox(nonce: AES.GCM.Nonce(data: nonce),
                                    ciphertext: ct, tag: tag)
    return try AES.GCM.open(box, using: aesKey, authenticating: aad)
}
```

### 6.6 Static Key Operational Limit

With a static `aes_key` and a 64-bit counter there is no practical risk of nonce exhaustion or
reuse (the counter is unique by construction). The NIST limit for AES-GCM with a single key is
~2³² messages before considering rotation: unreachable for this workload (approvals/clarifications).
Key rotation via **re-pairing** is nevertheless recommended if compromise is suspected.

---

## 7. `namespace_id` Derivation (NORMATIVE)

The namespace id is **immutably bound** to the agent's identity key — preventing takeover without
requiring relay-side state to guarantee it:

```
namespace_id_raw = SHA256( NS_DOMAIN ‖ 0x00 ‖ agent_ed25519_pub(32B) )   // 32 bytes
namespace_id     = hex(namespace_id_raw)                                  // 64 chars
```

- The relay, upon receiving the agent's auth, MUST verify that `namespace_id` derives from the
  presented `agent_ed25519_pub` and that the challenge signature is valid under that key.
- The client, from the QR, MUST verify `namespace_id == hex(SHA256(NS_DOMAIN ‖ 0x00 ‖ agent_ed25519_pub))`
  using the `agent_ed25519_pub` from the QR. This way it does not trust the relay for the id.
- `namespace_id_raw` is also the value used in the AAD (§6.2).

---

## 8. Challenge-Response (Key Ownership Proof)

On WS open the **relay speaks first** and sends a challenge. The connecting peer (any role) signs
and responds. Transport details in [relay-protocol.md](relay-protocol.md); here the primitive.

```
challenge_nonce = 32 random bytes (CSPRNG on the relay side), sent as raw bytes in protobuf
msg_to_sign     = AUTH_DOMAIN ‖ 0x00 ‖ challenge_nonce_raw(32B)
signature       = Ed25519_sign(ed25519_priv, msg_to_sign)        // 64 bytes
```

The relay verifies `Ed25519_verify(pub, signature, msg_to_sign)`. The **domain separation**
(`AUTH_DOMAIN`) prevents an auth signature from being reusable in other contexts.

```rust
let mut msg = Vec::with_capacity(20 + 32);
msg.extend_from_slice(b"skald-relay-auth-v1");
msg.push(0x00);
msg.extend_from_slice(&challenge_nonce_raw);     // 32B
let sig = ed25519_priv.sign(&msg);               // ed25519-dalek: Signer
```

```swift
var msg = Data("skald-relay-auth-v1".utf8); msg.append(0x00); msg.append(challengeNonceRaw)
let sig = try signingPriv.signature(for: msg)    // 64B
```

> Ed25519 internally hashes the message: do **not** pre-hash with SHA-256. Sign
> `AUTH_DOMAIN ‖ 0x00 ‖ nonce` directly.

---

## 9. Pairing Token (Capability Bearer, NOT a Signature)

The `pairing_token` is a **single-use bearer secret**, not a signature:

```
pairing_token = 32 random bytes (CSPRNG on the agent side), as raw bytes in protobuf
```

- The agent generates it on each `pairing_start`, puts it in the QR, and sends it to the relay
  (`PairingStart` frame). 256-bit entropy: not guessable.
- The relay treats it as an opaque blob: **byte-for-byte** comparison, **expiry**, **single-use**
  (consumed on first successful pairing), valid only while the namespace is in pairing mode.
- The client presents it in the pairing frame. It cannot verify it cryptographically (bearer token):
  security comes from **out-of-band QR** + **short TTL** + **single-use** + **explicit agent confirmation**
  of the new device.

> No Ed25519 signature on the token: nobody would verify it (security theater). A 256-bit random
> secret is simpler and equally strong as a capability.

---

## 10. Key Storage

### Agent (filesystem + DB)

```
data/relay/
└── seed                  # 32 bytes, 0600. The only persistent secret.
```

DB table `relay_clients` (see [../plugins/mobile-connector.md](../plugins/mobile-connector.md)):
stores per-client `x25519_pub`, `send_counter`, `recv_counter`. `shared_secret` and `aes_key`
are **never persisted**: re-derived from `seed` + `x25519_pub` on each startup (smaller attack
surface; negligible cost).

### Client (Keychain / Keystore)

- `seed` (32B) with `kSecAttrAccessibleWhenUnlockedThisDeviceOnly`, shared with the
  **Notification Service Extension** via **Keychain Access Group** (the NSE must be able to
  derive `aes_key`).
- `namespace_id`, `relay_url`, `agent_ed25519_pub`, `agent_x25519_pub`, `send_counter`,
  `recv_counter`: in the same shared storage.
- App uninstall → keys lost → re-pairing required.

---

## 11. Algorithm Summary

| Operation | Algorithm | Input → Output |
|-----------|-----------|----------------|
| Seed | CSPRNG | → 32B |
| Key derivation | HKDF-SHA256 (`KDF_SALT`, info) | seed 32B → x25519_priv 32B, ed25519_priv 32B |
| ECDH | X25519 | my_x25519_priv + peer_x25519_pub → shared 32B |
| AEAD key derivation | HKDF-SHA256 (`SESSION_SALT`, `SESSION_INFO`) | shared 32B → aes_key 32B |
| Encryption | AES-256-GCM | aes_key + nonce(DIR‖counter) + AAD(96B) → ciphertext‖tag |
| `namespace_id` | SHA-256 (`NS_DOMAIN`) | agent_ed25519_pub → 32B (hex) |
| Auth | Ed25519 sign/verify (`AUTH_DOMAIN`) | ed25519_priv + challenge → sig 64B |
| Pairing token | CSPRNG | → 32B single-use bearer |

## 12. Security Considerations

- **PFS**: not in the current protocol. Static `aes_key` → traffic capture + later seed theft =
  plaintext for historical messages. Roadmap: ephemeral ECDH per session.
- **Replay**: prevented by monotonic counter (§6.1) + `request_id` idempotency
  ([payloads.md](payloads.md)) + `ts` freshness.
- **Malicious relay**: cannot read content (E2E) and cannot relabel `from`/`to` (AAD, §6.2);
  it can only **drop/hold/reorder** → mitigated by fail-safe + TTL pending on the agent side.
- **Timing**: Ed25519 and AES-GCM are constant-time in the reference implementations
  (ed25519-dalek, aes-gcm with AES-NI feature, CryptoKit). Tag/token comparisons MUST be
  constant-time (`subtle` / `constantTimeAreEqual`).
- **Input validation**: reject malformed hex/base64, wrong lengths, and every failed decryption
  **without** distinguishing the cause in error messages.
