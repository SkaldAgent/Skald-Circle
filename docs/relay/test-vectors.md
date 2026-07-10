# Crypto Test Vectors — Interop

> Purpose: guarantee that **independent implementations** (relay/plugin in Rust, app in Swift,
> app in Kotlin) produce **the same bytes** from the same inputs. Without these vectors two
> "reasonable" implementations can silently diverge (KDF, byte order, AAD, nonce construction,
> plaintext framing) and never be able to decrypt each other's output.
>
> **Method (important):** the **source of truth** is the *reference generator* in §3 (Rust). The
> expected values in the tables MUST be produced by **running that tool** and then **committed**
> to this file. They are not hand-transcribed (manual transcription of crypto output causes
> errors). Every other implementation MUST reproduce those outputs exactly.
>
> **Framing:** the plaintext that is encrypted is not the raw JSON but a versioned envelope:
> `plaintext = version(0x01) ‖ comp(1B) ‖ payload(JSON)`. For payloads ≤ 1024 B, `comp = 0x00`
> (no compression). Vectors V14/V17 account for this framing — an implementor decrypting V14/V17
> must obtain the *framed* plaintext, then extract `plaintext[0]` (version), `plaintext[1]` (comp),
> and the payload.

Constants and encoding: [crypto.md §1](crypto.md), [index.md §5](index.md).

---

## 1. Fixed Inputs (deterministic)

All vectors start from these inputs. Bytes expressed in hex.

```
SEED_AGENT  (32B) = 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f
SEED_CLIENT (32B) = 202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f

CHALLENGE_NONCE (32B) = aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899

COUNTER_AGENT_TO_CLIENT = 1            // u64
COUNTER_CLIENT_TO_AGENT = 1            // u64

PLAINTEXT_A2C (inbox_update, agent→client), exact UTF-8:
{"v":1,"kind":"inbox_update","id":"00000000-0000-4000-8000-000000000001","ts":1750000000000,"badge":1,"approvals":[{"request_id":"appr_test_1","tool_name":"send_email","agent_label":"Skald","summary":"Test","created_at":1750000000000}],"clarifications":[]}

PLAINTEXT_C2A (approval_response, client→agent), exact UTF-8:
{"v":1,"kind":"approval_response","id":"00000000-0000-4000-8000-000000000002","ts":1750000000000,"request_id":"appr_test_1","decision":"approved"}
```

> The two plaintext strings are **fixed** JSON (no spaces, no field reordering) **only for the
> vector**: in production JSON is not canonicalised (it is encrypted as a blob and re-parsed).

## 2. Vector Table

| # | Value | Definition | Expected (hex / base64) |
|----|-------|-------------|------------------------|
| V1 | `agent_x25519_priv` | HKDF(SEED_AGENT, salt=`skald-kdf-v1`, info=`x25519`, 32) | `<gen>` |
| V2 | `agent_x25519_pub` | X25519(V1, base) | `<gen>` |
| V3 | `agent_ed25519_priv` | HKDF(SEED_AGENT, salt=`skald-kdf-v1`, info=`ed25519`, 32) | `<gen>` |
| V4 | `agent_ed25519_pub` | Ed25519 pub from V3 | `<gen>` |
| V5 | `client_x25519_priv` | HKDF(SEED_CLIENT, …, info=`x25519`, 32) | `<gen>` |
| V6 | `client_x25519_pub` | X25519(V5, base) | `<gen>` |
| V7 | `client_ed25519_priv` | HKDF(SEED_CLIENT, …, info=`ed25519`, 32) | `<gen>` |
| V8 | `client_ed25519_pub` | Ed25519 pub from V7 | `<gen>` |
| V9 | `namespace_id` | hex(SHA256(`skald-namespace-v1` ‖ 0x00 ‖ V4)) | `<gen>` |
| V10 | `shared_secret` | X25519(V1, V6) **==** X25519(V5, V2) | `<gen>` |
| V11 | `aes_key` | HKDF(V10, salt=`skald-session-v1`, info=`aes-256-gcm`, 32) | `<gen>` |
| V12 | `nonce_a2c` | `00000001` ‖ u64_be(1) = 12B | `000000010000000000000001` |
| V13 | `aad_a2c` (96B) | `ns_raw ‖ V4 ‖ V8` (ns_raw = raw 32B of SHA256, NOT hex; from=agent, to=client) | `<gen>` |
| V14 | `sealed_a2c` | AES-256-GCM.seal(V11, V12, V13, **PT_FRAMED_A2C**) = ct‖tag | `<gen base64>` |
| V15 | `nonce_c2a` | `00000002` ‖ u64_be(1) (12B) | `000000020000000000000001` |
| V16 | `aad_c2a` (96B) | `ns_raw ‖ V8 ‖ V4` | `<gen>` |
| V17 | `sealed_c2a` | AES-256-GCM.seal(V11, V15, V16, **PT_FRAMED_C2A**) | `<gen base64>` |
| V18 | `auth_sig_client` | Ed25519_sign(V7, `skald-relay-auth-v1` ‖ 0x00 ‖ CHALLENGE_NONCE) | `<gen>` |
| | **PT_FRAMED_A2C** | `0x01 ‖ 0x00 ‖ PLAINTEXT_A2C` ([framing.md §1](framing.md)) — what is fed to AES-GCM for V14 | 258B, see §3 |
| | **PT_FRAMED_C2A** | `0x01 ‖ 0x00 ‖ PLAINTEXT_C2A` ([framing.md §1](framing.md)) — what is fed to AES-GCM for V17 | 148B, see §3 |

V12 and V15 are deterministic by construction (already filled in). All other `<gen>` values
must be filled by running the tool in §3.

## 3. Reference Generator (Rust)

Lives in `crates/skald-relay-common` as the `gen-vectors` binary.

```sh
cargo run -p skald-relay-common --bin gen-vectors
```

The generator uses the shared library (`skald_relay_common::crypto`). The Rust snippet below is
a reference for independent implementations (Swift/Kotlin).

```rust
// Framing (framing.md §1):
//   plaintext = version(0x01) ‖ comp(1B) ‖ payload(JSON)
//   comp=0x00 for payload ≤ 1024 B (no compression)
//   What is encrypted is the FRAMED plaintext, not the raw JSON.
use hkdf::Hkdf; use sha2::{Sha256, Digest};
use ed25519_dalek::{SigningKey, Signer};
use x25519_dalek::{StaticSecret, PublicKey};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::{Aead, Payload}};
use base64::{Engine, engine::general_purpose::STANDARD as B64};

fn hkdf(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8;32] {
    let hk = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = [0u8;32]; hk.expand(info, &mut out).unwrap(); out
}
fn derive(seed: &[u8;32]) -> (StaticSecret, [u8;32], SigningKey, [u8;32]) {
    let x = StaticSecret::from(hkdf(seed, b"skald-kdf-v1", b"x25519"));
    let xp = PublicKey::from(&x).to_bytes();
    let e = SigningKey::from_bytes(&hkdf(seed, b"skald-kdf-v1", b"ed25519"));
    let ep = e.verifying_key().to_bytes();
    (x, xp, e, ep)
}
fn frame_payload(payload: &[u8]) -> Vec<u8> {
    let mut framed = vec![0x01u8]; // version
    framed.push(0x00); // comp = none (payload < 1024B)
    framed.extend_from_slice(payload);
    framed
}
fn main() {
    let seed_a: [u8;32] = (0u8..32).collect::<Vec<_>>().try_into().unwrap();
    let seed_c: [u8;32] = (32u8..64).collect::<Vec<_>>().try_into().unwrap();
    let (xa, xa_pub, ea, ea_pub) = derive(&seed_a);
    let (xc, xc_pub, ec, ec_pub) = derive(&seed_c);

    let mut h = Sha256::new();
    h.update(b"skald-namespace-v1"); h.update([0u8]); h.update(ea_pub);
    let ns_raw = h.finalize();
    let ns_hex = hex::encode(ns_raw);

    let s1 = xa.diffie_hellman(&PublicKey::from(xc_pub));
    let s2 = xc.diffie_hellman(&PublicKey::from(xa_pub));
    assert_eq!(s1.as_bytes(), s2.as_bytes(), "ECDH mismatch");
    let aes_key = hkdf(s1.as_bytes(), b"skald-session-v1", b"aes-256-gcm");
    let cipher = Aes256Gcm::new((&aes_key).into());

    let mut n_a2c = [0u8;12]; n_a2c[..4].copy_from_slice(&[0,0,0,1]);
    n_a2c[4..].copy_from_slice(&1u64.to_be_bytes());
    let mut aad_a2c = Vec::new(); aad_a2c.extend_from_slice(&ns_raw);
    aad_a2c.extend_from_slice(&ea_pub); aad_a2c.extend_from_slice(&ec_pub);

    let pt_a2c = br#"{"v":1,"kind":"inbox_update","id":"00000000-0000-4000-8000-000000000001","ts":1750000000000,"badge":1,"approvals":[{"request_id":"appr_test_1","tool_name":"send_email","agent_label":"Skald","summary":"Test","created_at":1750000000000}],"clarifications":[]}"#;
    let framed_a2c = frame_payload(pt_a2c);
    let sealed_a2c = cipher.encrypt(Nonce::from_slice(&n_a2c),
        Payload{ msg: &framed_a2c, aad: &aad_a2c }).unwrap();

    let mut m = Vec::new(); m.extend_from_slice(b"skald-relay-auth-v1"); m.push(0);
    m.extend_from_slice(&hex::decode("aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899").unwrap());
    let sig = ec.sign(&m);

    println!("V2  agent_x25519_pub   = {}", hex::encode(xa_pub));
    println!("V4  agent_ed25519_pub  = {}", hex::encode(ea_pub));
    println!("V6  client_x25519_pub  = {}", hex::encode(xc_pub));
    println!("V8  client_ed25519_pub = {}", hex::encode(ec_pub));
    println!("V9  namespace_id       = {}", ns_hex);
    println!("V10 shared_secret      = {}", hex::encode(s1.as_bytes()));
    println!("V11 aes_key            = {}", hex::encode(aes_key));
    println!("V13 aad_a2c            = {}", hex::encode(&aad_a2c));
    println!("V14 sealed_a2c (b64)   = {}", B64.encode(&sealed_a2c));
    println!("V18 auth_sig_client    = {}", hex::encode(sig.to_bytes()));
    println!("# PT_FRAMED_A2C = {}", hex::encode(&framed_a2c));
}
```

---

## 4. Canonical Outputs (committed once)

```
# Generated by `cargo run -p skald-relay-common --bin gen-vectors`
# Framing (framing.md §1): the bytes fed to AES-GCM are
# plaintext = version(0x01) ‖ comp(1B) ‖ payload(JSON).
# Below threshold (1024B), comp = 0x00. V14/V17 seal the FRAMED plaintext.
V1  agent_x25519_priv  = 497a4febd79a47e0a0b9522273ef8db2588b113e3d58365e4462e0899b932495
V2  agent_x25519_pub   = 4fcb9922300372851653f0d8a0d48855674b6f6095e3770273d212bcaf51bc64
V3  agent_ed25519_priv = 13b9de6a991a9d382dec70bdeb7d8b36327ebcb81a45fa7ac7829376a695f433
V4  agent_ed25519_pub  = b3e202f4ac99fd9929da47df20adedd5b2598411a466a229f086eda3467ffa7b
V5  client_x25519_priv = 5cc48fd4f6fa941053037ba6b8b1ed1daad48764d0084670307d79c4809b28a8
V6  client_x25519_pub  = fc472466d9013da9a50a49b6031cde99c1cfd11c87ee04fe4da952417a1f7337
V7  client_ed25519_priv= cbaabfd5b937657cf4e7964ba87c975401337f3ce0d27026a404f102bd7c68c8
V8  client_ed25519_pub = 12355ea750e60d6370ba6776037f25062f6c9450c5009669884895fd5b377a18
V9  namespace_id       = f7d340d3c3f0b0052fa904ba60ebd38a0f7e7d10672ac80648991a2c632c9e58
V10 shared_secret      = 66c51034dd6360b9cdddc495049463b0191d7f3bddce9ea6f2975c85d471540a
V11 aes_key            = 74fb4ffcbbe069859cfb0790023811554dad328d9f4ac4a1d28077086e33a4e7
V12 nonce_a2c          = 000000010000000000000001
V13 aad_a2c            = f7d340d3c3f0b0052fa904ba60ebd38a0f7e7d10672ac80648991a2c632c9e58b3e202f4ac99fd9929da47df20adedd5b2598411a466a229f086eda3467ffa7b12355ea750e60d6370ba6776037f25062f6c9450c5009669884895fd5b377a18
V14 sealed_a2c (b64)   = FrtkSke7RpPUAg24p1XPZpswSX3WoDv/Y2IUvvaahY5+2CcdHXKvyRhsdjqCVa7zVs9Y0a4SZ1a7ddsPKYPz0BX/Ur3nDOOwTySKaDqT8fca//XpJyVkd60TxbfZkILNejruBLX7y2he3OI6MYu2TrmgmUSrqqfJ6NX9Go5gaKoyenXoVKOY3NKuSNmIEyIzYEkZj8uImEgah9BG/6lI59a1LWfJDlgggFf5KWkoPJHHAHA4546aPFEk5iG+3WLcjq6yiiE0p/umsr5jG2AjnkvVWYpYe8paZ4sWy/HkIYkzo9zJAGnmvK9UBHJupZABSioeRYFW2WN6ierUHbp2WyQxYvcb0x/K73Lmp4hSg6DS3w==
V15 nonce_c2a          = 000000020000000000000001
V16 aad_c2a            = f7d340d3c3f0b0052fa904ba60ebd38a0f7e7d10672ac80648991a2c632c9e5812355ea750e60d6370ba6776037f25062f6c9450c5009669884895fd5b377a18b3e202f4ac99fd9929da47df20adedd5b2598411a466a229f086eda3467ffa7b
V17 sealed_c2a (b64)   = WYOy3vzVD+DI6lZQ4atH8g2yPfcgSo9uNNsfkWUoRD+KXWaKlDaazN6AmYAM+S3tGEVimk1HedYUJ4QrzBZJYoeBUYSxiz7WpRnqgD9mumHp8GCypttt9+/FNc7tc/zLERvtW2GfsVJSKrs0MpKFTNCauoYLdFuKdWy/A2QykrZXlySbwaNXPnMOA3ApeEsPidPHutom7G6ksgSz0qhuceIbNt4=
V18 auth_sig_client    = ae38491a1f25bb5fb11f0b17e3d344412bfc927461b6517e9a0ab6a64020054677f59490af026f34c81d9378d4daae4823109ca2d1afbf4ff00230a038270002

# Framed plaintexts (input to AES-GCM, framing.md §1):
PT_FRAMED_A2C = 01007b2276223a312c226b696e64223a22696e626f785f757064617465222c226964223a2230303030303030302d303030302d343030302d383030302d303030303030303030303031222c227473223a313735303030303030303030302c226261646765223a312c22617070726f76616c73223a5b7b22726571756573745f6964223a22617070725f746573745f31222c22746f6f6c5f6e616d65223a2273656e645f656d61696c222c226167656e745f6c6162656c223a22536b616c64222c2273756d6d617279223a2254657374222c22637265617465645f6174223a313735303030303030303030307d5d2c22636c6172696669636174696f6e73223a5b5d7d
PT_FRAMED_C2A = 01007b2276223a312c226b696e64223a22617070726f76616c5f726573706f6e7365222c226964223a2230303030303030302d303030302d343030302d383030302d303030303030303030303032222c227473223a313735303030303030303030302c22726571756573745f6964223a22617070725f746573745f31222c226465636973696f6e223a22617070726f766564227d
# framed_a2c[:2] = 0100  (version=01, comp=00 = none for <1024 B)
# framed_c2a[:2] = 0100  (version=01, comp=00 = none for <1024 B)
# PT_FRAMED_A2C.len = 258  (PLAINTEXT_A2C.len + 2 framing header bytes)
# PT_FRAMED_C2A.len = 148  (PLAINTEXT_C2A.len + 2 framing header bytes)
```

Once committed, these values are **immutable**. If they change after a library update, it is a
**bug** (likely a KDF/encoding/framing divergence): investigate, do not blindly update.

> **Interop invariant:** the relay's `verify_strict` MUST accept signatures produced by the iOS
> client. Verified by cross-compat tests in
> `crates/skald-relay-server/src/auth.rs::tests::challenge_verifies_cryptokit_signature` and
> `SkaldInboxTests/SkaldInboxTests.swift::testAuthSignatureCrossCompatWithDalek`.

---

## 5. Swift Verification (CryptoKit)

Unit test in the app: derive from `SEED_AGENT`/`SEED_CLIENT` and **assert** equality with §4.

```swift
func testInteropVectors() throws {
    let seedA = Data((0..<32).map { UInt8($0) })
    let (signA, agreeA) = deriveKeys(seed: seedA)          // crypto.md §3
    XCTAssertEqual(agreeA.publicKey.rawRepresentation.hex, "<V2>")
    XCTAssertEqual(signA.publicKey.rawRepresentation.hex,  "<V4>")

    let seedC = Data((32..<64).map { UInt8($0) })
    let (signC, agreeC) = deriveKeys(seed: seedC)
    let shared = try agreeC.sharedSecretFromKeyAgreement(with: agreeA.publicKey)
    let key = shared.hkdfDerivedSymmetricKey(using: SHA256.self,
                salt: Data("skald-session-v1".utf8),
                sharedInfo: Data("aes-256-gcm".utf8), outputByteCount: 32)
    // Decrypt: strip framing header from the decrypted bytes, compare with PLAINTEXT_A2C
    // open(sealed=base64(<V14>), nonce=<V12>, aad=<V13>) → plaintext_framed
    // plaintext_framed[2:] == PLAINTEXT_A2C
}
```

If **even one** vector does not match, the app will not be interoperable: fix it before
continuing.

---

## 6. Interop Checklist (for each implementation)

- [ ] V2/V4/V6/V8: same pubkeys from seed → **identical KDF derivation**.
- [ ] V9: same `namespace_id` → **correct domain + byte order**.
- [ ] V10: ECDH symmetric and equal → **correct X25519** (no ed25519-as-x25519).
- [ ] V11: same `aes_key` → **correct session HKDF**.
- [ ] V14/V17: mutually decryptable → **correct nonce(DIR‖counter) + AAD + GCM**.
- [ ] V14/V17: decrypted framed plaintext starts with `0x01 0x00`, remainder == PLAINTEXT_*
      → **framing.md §1 implemented correctly**.
- [ ] V18: valid and reproducible signature → **correct auth domain separation**.
- [ ] Cross-language round-trip: app decrypts a `sealed` produced by the Rust plugin and vice versa.
