# Skald Remote Control — Architecture & Index

> **Purpose.** Specify, unambiguously, how to build the system that lets a mobile app (iOS/Android)
> remotely control a person's **Skald instance** — even when Skald runs at home behind NAT.
> Documents are written as **implementation contracts**: a coding agent must be able to implement
> its component (relay, plugin, app) by reading only these files and achieve byte-for-byte
> interoperability with all other components.

## 1. The Problem

Skald is self-hosted: anyone who installs it locally ends up **behind NAT**, unreachable from the
internet. We want a mobile app that:

1. receives **push notifications** when Skald needs human input (approvals, clarifications);
2. **responds** (approve / reject / clarify) even with Skald behind NAT.

Push notification systems (APNs/FCM) do not allow an arbitrary sender to push to someone else's
app: a component holding the push credentials is required. Hence the **relay**.

The entire architecture exists **only** to solve: (a) bidirectional communication through NAT,
(b) push notifications. Nothing more. The relay is designed to be **content-blind**.

> **What this is NOT.** Not a chat, not a streaming system, not a sub-agent protocol.
> The mobile client is a **remote control surface** (a human-in-the-loop remote) for the
> **single Skald instance** that owns the namespace. The approvals and clarifications the client
> sees are those exposed by that Skald instance through its Inbox; how Skald generates them
> internally (tools, scheduled jobs, etc.) is an internal detail outside this spec.

## 2. Actors

| Actor | Abbr | Role |
|-------|------|------|
| **Skald Agent** | `agent` | The Skald instance. **Namespace owner.** Holds the identity key. Opens a permanent WS connection to the relay. Encrypts/decrypts E2E. |
| **Relay Client** | `agent` impl | `crates/skald-relay-client/`: the **standalone, payload-agnostic** library that implements the `agent` role — keys, WS v2 transport, E2E crypto, anti-replay counters, pairing, device authorization, SQLite persistence. Exchanges opaque decrypted bytes via `RelayEvent`; depends only on `skald-relay-common` (never on Skald/`core-api`). |
| **Mobile Connector Plugin** | — | The thin **application** crate inside Skald (`crates/plugin-mobile-connector/`) on top of the relay client: it owns the JSON payload schemas, the Inbox↔relay routing, the authorization policy, and the QR endpoint. The bridge to mobile apps; today via relay, in the future also via direct transports (TCP/port-forward). See [server.md](server.md) and [../plugins/mobile-connector.md](../plugins/mobile-connector.md). |
| **Relay Server** | `relay` | The only centralised component. APNs/FCM bridge, store-and-forward, namespace routing. **Zero-trust on content.** See [server.md](server.md). |
| **Shared Crate** | — | `crates/skald-relay-common/`: protocol frame types (protobuf) + cryptographic primitives, shared **byte-for-byte** between relay, relay client, and server (no duplication). |
| **Client** | `client` | Mobile app (iOS/Android). Pairs via QR, encrypts/decrypts E2E, shows Inbox, responds. Implementation documented in the iOS app repository. |

A **namespace** is the isolated zone of one person: their agent + their authorised clients.
Different namespaces are unaware of each other. Multiple devices can share a namespace
(iPhone + iPad).

## 3. Architecture

```
        Home / NAT                         Cloud                         Pocket
┌───────────────────────┐        ┌────────────────────────┐     ┌──────────────────────┐
│   Skald Agent          │        │     Relay Server        │     │  Client (iOS/Android) │
│   (namespace owner)    │        │     (zero-trust)        │     │                       │
│  ┌──────────────────┐  │  WSS   │  • APNs/FCM bridge      │ WSS │  ┌─────────────────┐  │
│  │ Mobile Connector │◀─┼───────▶│  • store-and-forward    │◀───▶│  │ CryptoEngine     │  │
│  │ ed25519 + X25519 │  │ (perm.)│  • namespace routing    │     │  │ ed25519 + X25519 │  │
│  └──────────────────┘  │        │  • does NOT decrypt     │     │  └─────────────────┘  │
└───────────────────────┘        └───────────┬────────────┘     └──────────────────────┘
                                              │ push (wake / encrypted blob)
                                              ▼
                                       APNs (Apple) / FCM (Google)
```

- All actors connect to the **same** WebSocket endpoint on the relay.
- Agent↔client communication is **end-to-end encrypted**: the relay sees only opaque blobs.
- The relay routes by public key within the namespace and, if the recipient is offline,
  queues and sends a push.

## 4. Threat Model (read before implementing)

### 4.1 Guarantees

| Guarantee | Mechanism |
|-----------|-----------|
| **Content confidentiality** end-to-end | AES-256-GCM with key derived from ECDH X25519. The relay has no key. |
| **Content integrity + authenticity** | GCM tag + binding of `from`/`to`/`namespace_id` in AAD. A relay that flips one byte breaks decryption. |
| **Peer authentication at pairing** | The agent's X25519 public key arrives **out-of-band** via QR (TOFU). The E2E channel is authenticated toward whoever controls that key. |
| **Anti-replay** | Per-direction **monotonic counter** nonce + `request_id` idempotency + `ts` freshness. See [crypto.md](crypto.md). |
| **Key ownership proof** (to the relay) | Challenge-response with Ed25519 signature, with domain separation. |
| **No namespace takeover** | `namespace_id = SHA256(domain ‖ agent_ed25519_pub)`: the id is immutably bound to the key. |
| **Device authorisation controlled by the owner** | Only the agent decides the authorised list. Pairing produces a **pending** device until the agent confirms. Pairing token is **single-use**. |

### 4.2 What the Relay CAN See and Do (declared limits)

> "Zero-trust" here means **content-confidential**, **not** metadata-private. This must be stated
> explicitly in the privacy policy.

| The relay sees | Notes |
|----------------|-------|
| Public keys of agent and clients | Public identifiers, not linked to real identities. |
| `device_token` (APNs/FCM), `platform` | Required for push delivery. |
| IP addresses (TCP/TLS layer) | Unavoidable. |
| Relationship graph (who talks to whom), timing, message sizes | Routing metadata. The relay learns **when** you are active. |

| The relay does NOT see | Why |
|------------------------|-----|
| Content / message type | E2E encrypted; the AAD is authenticated but the routing fields are only pubkeys. |
| Detailed `device_info` (model, OS, app version) | Sent **E2E** to the agent after pairing (`hello`), not to the relay. |

| The relay CAN do (and we defend against it) | Defence |
|---------------------------------------------|---------|
| **Drop / hold / reorder** messages and pushes | A lost approval = no action (fail-safe). Pending items have **TTL on the agent side**: a held-then-released "approve" is **no longer acted upon** after expiry. |
| **Replay** an encrypted blob | Monotonic counter per direction + `request_id` idempotency: a replay is discarded. |
| **Relabel** `from`/`to` | `from`/`to`/`namespace_id` are in the GCM AAD: decryption fails. |

### 4.3 Out of Scope (assumptions)

- **Compromised host** (agent or device): if the attacker has the seed, they have everything. Unavoidable.
  Mitigation: minimal-permission storage / Keychain `ThisDeviceOnly`.
- **Apple/Google push channel compromise**: content stays E2E-protected; at worst availability is lost.
- **Perfect Forward Secrecy**: **not** in the current protocol (static shared secret after pairing). Roadmap.
  Accepted consequence: traffic capture + later seed theft = plaintext for historical messages.

## 5. Encoding Conventions (NORMATIVE — apply to all files)

To eliminate ambiguity between implementations, the encoding of **every** binary field is fixed here.

| Data type | Wire encoding (JSON) | Example |
|-----------|----------------------|---------|
| Public keys (ed25519, X25519), 32 bytes | **lowercase hex**, 64 chars | `"3b6a…"` |
| Ed25519 signatures, 64 bytes | **lowercase hex**, 128 chars | `"9f1c…"` |
| `namespace_id` (SHA-256, 32 bytes) | **lowercase hex**, 64 chars | `"a17e…"` |
| `pairing_token` (32 bytes random) | **lowercase hex**, 64 chars | `"5d20…"` |
| Challenge `nonce` (32 bytes random) | **lowercase hex**, 64 chars | `"c4f0…"` |
| AEAD `nonce` (12 bytes) | **lowercase hex**, 24 chars | `"000000016a…"` |
| **Ciphertext** AEAD (variable, ciphertext‖tag) | **standard base64 with padding** (RFC 4648 §4) | `"q1B2…=="` |

Rules:

1. **Hex for fixed-length material** (keys, signatures, ids, nonces): easy to compare and debug.
   Hex MUST always be lowercase; an implementation receiving uppercase MUST accept it but MUST emit lowercase.
2. **Standard base64 (not url-safe), with padding** for variable-length blobs (only ciphertext qualifies).
3. These rules apply to **JSON payloads** (the E2E content). The relay transport layer uses protobuf
   binary frames where all binary fields travel as **raw bytes** — no hex, no base64.
4. Application timestamps: **unix epoch in milliseconds** (integer). Relay routing timestamps:
   ISO-8601 UTC string (advisory only).
5. Unknown fields in JSON are ignored (forward-compat). Integers without decimal point.

## 6. Document Map

| File | Content | Primary audience |
|------|---------|-----------------|
| [index.md](index.md) | This file: vision, actors, threat model, encoding | Everyone |
| [crypto.md](crypto.md) | **Crypto contract**: seed, key derivation, ECDH, HKDF, AEAD, AAD, anti-replay, signatures | All implementors |
| [relay-protocol.md](relay-protocol.md) | **WebSocket protocol**: protobuf transport, auth, pairing, message envelope, live channel, presence, errors, limits | Relay, plugin, app |
| [framing.md](framing.md) | **E2E plaintext framing** `[version][comp][payload]` + optional zlib compression | Plugin, app |
| [pipe.md](pipe.md) | **Relayed byte-stream** (TURN-style): control-plane signaling + `/v1/pipe` data plane, per-pipe ephemeral DH (PFS), splice + limits | Relay, relay client, app |
| [payloads.md](payloads.md) | **E2E payload schemas** (the encrypted content the relay never sees) | Plugin, app |
| [describe-and-push.md](describe-and-push.md) | **Approval rendering**: `summary` + structured `blocks`, push delivery model | Plugin, app |
| [server.md](server.md) | **Relay server** implementation (Rust): zero-trust, store-and-forward, push bridge, deploy | Relay coding agent |
| [test-vectors.md](test-vectors.md) | **Crypto test vectors** + reference generator for byte-for-byte interop | All implementors |

> **Recommended reading order for a coding agent:** index → crypto → relay-protocol → framing →
> payloads → (your component's file) → test-vectors.

## 7. Versioning

- Protocol version in the URL: `/v1/ws`. Payload schema version in the `v` field (integer) of each
  E2E JSON.
- Crypto domain constants (salt/info/prefix) contain `v1`. A future protocol would use different
  constants: no cross-version confusion possible.
- **All** normative constants live in [crypto.md §1](crypto.md). No other file redefines them.
- The WebSocket transport uses **protobuf binary frames** (`RelayFrame`, package `skald.relay.v2`)
  with raw bytes for all binary fields. The proto schema lives in `crates/skald-relay-common`.
- E2E plaintext framing is versioned by the `version` byte (`0x01` = JSON app payload, `0x02` = pipe
  signaling), independently of the JSON payload schema version (`v` field). See [framing.md](framing.md).
- The pipe data plane adds **one** endpoint, `/v1/pipe` (relayed byte-stream). See [pipe.md](pipe.md).

## 8. Links

- Skald backend: `crates/` (workspace root)
- Shared crate: `crates/skald-relay-common/`
- Mobile connector plugin: `crates/plugin-mobile-connector/`
- Relay server: `crates/skald-relay-server/`
- iOS app: `/Users/dguiducci/projects/skald-ios/` (target `SkaldInbox` + Notification Service Extension)
- iOS skill: `skills/ios-development/SKILL.md`
