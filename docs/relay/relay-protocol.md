# Relay Protocol — WebSocket

> Transport protocol between **any actor** (agent, client, pairing) and the **relay**, over
> **a single WebSocket**. No REST. This file defines the **protobuf frame schema**, the
> **authentication handshake**, the **E2E message envelope**, the **live channel**, **presence**,
> and the **pairing flow**. The **encrypted content** inside the envelope is in
> [payloads.md](payloads.md); the **cryptography** is in [crypto.md](crypto.md).
>
> MUST/SHOULD carry the RFC 2119 meaning.

**Transport**: every WebSocket frame is a **binary frame** (opcode `0x2`) carrying exactly one
`RelayFrame` protobuf. All binary fields (keys, signatures, nonces, namespace_id) travel as
**raw bytes** — no hex, no base64. The encoding rules in [index.md §5](index.md) apply only
inside the E2E JSON payloads, not to the transport layer.

---

## 1. Concepts

- **Namespace**: created implicitly when an `agent` authenticates for the first time. Identified by
  `namespace_id = hex(SHA256(NS_DOMAIN ‖ 0x00 ‖ agent_ed25519_pub))` ([crypto.md §7](crypto.md)).
  Expires after **7 days** without any connection.
- **Owner**: the `agent` holding the namespace private key. **Sole authority** over the authorised
  client list.
- **Client**: a mobile device **authorised by the agent**. Before pairing it does not exist; after
  pairing it is `pending` until the agent authorises it.

## 2. Endpoint

```
wss://<relay-host>/v1/ws
```

Single endpoint for all actors. The **role** is established in the `Auth` frame. `namespace_id`
is NOT in the query string: it travels inside `Auth`. Transport: **WSS mandatory** (TLS); the
relay MUST reject plain WS.

## 3. RelayFrame Schema (NORMATIVE)

Lives in `crates/skald-relay-common`, generated for Rust (`prost`) and iOS (`SwiftProtobuf`).
Package name: `skald.relay.v2`.

```proto
syntax = "proto3";
package skald.relay.v2;

// One WebSocket binary frame = one RelayFrame.
message RelayFrame {
  oneof frame {
    Challenge       challenge        = 1;
    Auth            auth             = 2;
    AuthOk          auth_ok          = 3;
    AuthError       auth_error       = 4;
    Authorize       authorize        = 5;
    AuthorizeOk     authorize_ok     = 6;
    PairingStart    pairing_start    = 7;
    PairingReady    pairing_ready    = 8;
    PairingStop     pairing_stop     = 9;
    PairingStopOk   pairing_stop_ok  = 10;
    ClientPaired    client_paired    = 11;
    Message         message          = 12;
    PeerOffline     peer_offline     = 13;
    PresenceRequest presence_request = 14;
    PresenceList    presence_list    = 15;
    PresenceEvent   presence_event   = 16;
    Error           error            = 17;
  }
  reserved 18, 19;   // ex Ping/Pong: keepalive via native WS frames (§8), not protobuf
}

// --- Data plane (E2E). The relay routes, does NOT read ciphertext/nonce. ---
message Message {
  bytes ciphertext = 1;   // E2E payload: JSON+framing (framing.md). Opaque to relay.
  bytes nonce      = 2;   // 12B, AEAD nonce
  bytes peer       = 3;   // 32B: 'to' on send (sender→relay), 'from' on delivery (relay→dest)
  bool  live       = 4;   // true = live channel (§6): route-or-fail, no queue/push
}
message PeerOffline { bytes peer = 1; }   // 32B: recipient not connected (live channel only)

// --- Presence (§7). Control frames, not E2E. ---
message PresenceRequest {}
message PresenceList  { repeated bytes online = 1; }            // 32B each
message PresenceEvent { bytes pubkey = 1; Status status = 2; }  // 32B
enum Status { STATUS_UNSPECIFIED = 0; STATUS_ONLINE = 1; STATUS_OFFLINE = 2; }

// --- Handshake / auth / pairing: role is implicit in the sub-message set.
//     No enum Role (its default 0 would mean "AGENT" — security footgun). ---
message Challenge { bytes nonce = 1; }                           // 32B

message Auth {
  oneof role {
    AuthAgent   agent   = 1;
    AuthClient  client  = 2;
    AuthPairing pairing = 3;
  }
  bytes signature = 4;   // 64B, over AUTH_DOMAIN‖0x00‖nonce
}
message AuthAgent  { bytes agent_ed25519_pub = 1; }              // 32B; namespace_id = hash(pubkey)
message AuthClient {
  bytes    namespace_id       = 1;   // 32B
  bytes    client_ed25519_pub = 2;   // 32B
  string   device_token       = 3;   // push token (opaque)
  Platform platform           = 4;
}
message AuthPairing {
  bytes    namespace_id       = 1;   // 32B
  bytes    client_ed25519_pub = 2;   // 32B
  bytes    client_x25519_pub  = 3;   // 32B
  bytes    pairing_token      = 4;   // 32B
  string   device_token       = 5;   // push token (opaque)
  Platform platform           = 6;
}
enum Platform { PLATFORM_UNSPECIFIED = 0; PLATFORM_IOS = 1; PLATFORM_ANDROID = 2; }

message AuthOk    { bytes namespace_id = 1; }
message AuthError { string code = 1; string message = 2; }
message Authorize   { repeated bytes clients = 1; }              // 32B each (replaces full list)
message AuthorizeOk { uint32 authorized = 1; }
message PairingStart  { bytes pairing_token = 1; uint32 ttl = 2; }
message PairingReady  { uint32 ttl = 1; }
message PairingStop   {}
message PairingStopOk {}
message ClientPaired  {
  bytes    client_ed25519_pub = 1;
  bytes    client_x25519_pub  = 2;
  Platform platform           = 3;
}
message Error { string code = 1; string message = 2; }
// Keepalive: native WebSocket ping/pong frames (§8), not protobuf messages.
```

> **Validation (proto3 has no `required`).** The role split prevents cross-role confusion but
> does not enforce non-empty fields: the relay MUST still validate the **presence and length** of
> `bytes` fields (32B pubkeys, 64B signatures, …) and reject with `bad_request`. The
> `*_UNSPECIFIED = 0` enum values make "absent enum field" distinguishable and rejectable.

## 4. Authentication Handshake

**The relay speaks first.** As soon as the WS is open, it sends a `Challenge`. Until `AuthOk`
arrives, the only frame accepted from the peer is `Auth`.

```
PEER (agent | pairing | client)                   RELAY
   │  ── WSS connect ───────────────────────────── ▶│
   │  ◀──── Challenge { nonce: 32B } ───────────────│  relay speaks first
   │  ── Auth { role:..., signature: 64B } ────────▶│
   │  ◀──── AuthOk { namespace_id } ────────────────│
   │     OR AuthError { code, message } ────────────│
```

- `Challenge.nonce`: 32 random bytes. Unique per connection. Expires after **30 s**: no `Auth`
  in time → `challenge_timeout` and close.
- `Auth.signature`: Ed25519 signature of `AUTH_DOMAIN ‖ 0x00 ‖ nonce_raw` ([crypto.md §8](crypto.md)).
- The relay MUST verify the signature under the role-appropriate public key **before** any other
  logic.

### 4.1 `role: agent` — the Skald instance

The namespace may not exist yet: it is created here.

```proto
Auth {
  agent: AuthAgent { agent_ed25519_pub: <32B> },
  signature: <64B>
}
```

Relay checks (in order):
1. `agent_ed25519_pub` is exactly 32 bytes.
2. `signature` valid over `AUTH_DOMAIN ‖ 0x00 ‖ nonce_raw` under `agent_ed25519_pub`.
3. Compute `namespace_id = SHA256(NS_DOMAIN ‖ 0x00 ‖ agent_ed25519_pub)`.
4. If namespace doesn't exist → create it (bind `namespace_id ↔ agent_ed25519_pub`, immutable).
   If it exists → pubkey MUST match (by construction it does, since the id is a hash of the key;
   a mismatch is a bug → `not_found`).
5. If an `agent` WS is already open for this namespace → close the old one (one agent connection
   per namespace at a time).

Response: `AuthOk { namespace_id: <32B raw> }`.

Right after, the agent SHOULD send an `Authorize` frame (§5) with the current authorised client
list (possibly empty).

### 4.2 `role: pairing` — not-yet-authorised client

For initial connection before authorisation. Accepted only if the namespace is in **pairing mode** (§9).

```proto
Auth {
  pairing: AuthPairing {
    namespace_id: <32B>,
    client_ed25519_pub: <32B>,
    client_x25519_pub: <32B>,
    pairing_token: <32B>,
    device_token: "<push token>",
    platform: PLATFORM_IOS | PLATFORM_ANDROID
  },
  signature: <64B>
}
```

Relay checks:
1. `signature` valid under `client_ed25519_pub`.
2. `namespace_id` exists and is in pairing mode.
3. `pairing_token` matches **byte-for-byte** the one from `PairingStart`, **not expired**,
   **not yet consumed** (single-use).
4. Mark the token **consumed**. Register the client as **`pending`** (NOT yet authorised):
   store `client_ed25519_pub`, `client_x25519_pub` (opaque), `device_token`, `platform`.
5. Forward a `ClientPaired` frame to the agent (§9.4).

Response: `AuthOk { namespace_id: <32B raw> }`.

After `AuthOk` the pairing client **closes** the WS. It becomes operational by reconnecting with
`role: client` **once the agent has authorised it** (the app may retry with backoff until it
receives `AuthOk` instead of `unauthorized`).

> `device_token` and `platform` are the **only** device data the relay knows: required for push.
> Model, OS, app version do NOT pass through the relay: the app sends them **E2E** to the agent
> via a `hello` message ([payloads.md](payloads.md)).

### 4.3 `role: client` — authorised device

```proto
Auth {
  client: AuthClient {
    namespace_id: <32B>,
    client_ed25519_pub: <32B>,
    device_token: "<push token>",
    platform: PLATFORM_IOS | PLATFORM_ANDROID
  },
  signature: <64B>
}
```

Relay checks:
1. `signature` valid under `client_ed25519_pub`.
2. `namespace_id` exists.
3. `client_ed25519_pub` is in the **authorised** list (NOT `pending`). Otherwise `unauthorized`.
4. Update `device_token` (it can change: APNs/FCM rotate it).
5. If a `client` WS is already open for the same pubkey → close the old one.
6. Deliver any queued messages (store-and-forward, §6.3).

Response: `AuthOk { namespace_id: <32B raw> }`.

## 5. Client Authorisation (agent only)

The agent is the **sole authority**. The authorised list is declared with:

```proto
Authorize { clients: [ <32B>, <32B>, … ] }
```

- **Replacement semantics**: this list **replaces** the previous one (not an append). To add a
  device, send the full list including it; to revoke one, send the list without it.
- Relay effects, atomic:
  - keys present now but absent before → become `authorised` (exit `pending`);
  - keys absent now but present before → **revoked**: the relay MUST (a) close that client's active
    WS if any, (b) **purge its store-and-forward queue**, (c) forget its `device_token`.
- Response: `AuthorizeOk { authorized: N }` (N = number of active authorised clients).

## 6. E2E Messages

After `AuthOk`, agent and client exchange **opaque** messages routed by pubkey.

### 6.1 Sending (sender → relay)

```proto
Message {
  ciphertext: <bytes>,   // E2E blob: framed JSON, encrypted AES-256-GCM
  nonce:      <12B>,
  peer:       <32B>,     // destination ed25519 pubkey
  live:       false      // or true for live channel (§6.4)
}
```

- `peer` = ed25519 pubkey of the recipient (the agent, or a client).
- The relay knows the sender: it is the pubkey authenticated on **this** WS. It does NOT trust any
  `from` field supplied by the sender.
- The relay MUST verify that `peer` belongs to the **same namespace** (namespace agent, or an
  authorised client). Otherwise `not_found`.
- The relay NEVER reads or alters `nonce` and `ciphertext`.

### 6.2 Receiving (relay → recipient)

The relay rewrites `Message.peer` from `to` (the destination sent by the sender) to `from`
(the authenticated pubkey of the sender, which the relay guarantees), and adds a routing
`timestamp` (advisory, ISO-8601 UTC) via delivery metadata if needed.

```proto
Message {
  ciphertext: <bytes>,
  nonce:      <12B>,
  peer:       <32B>,     // 'from': authenticated sender pubkey
  live:       <bool>
}
```

The recipient:
1. reconstructs the AAD = `namespace_id_raw ‖ peer_pub(from) ‖ my_pub` ([crypto.md §6.2](crypto.md));
2. selects the `aes_key` for peer `from`;
3. decrypts; verifies the **counter** in the nonce (`> last_seen`, [crypto.md §6.1](crypto.md));
4. strips the framing header ([framing.md](framing.md)), parses payload ([payloads.md](payloads.md)).
   Idempotent by `request_id`.

### 6.3 Store-and-Forward (`live=false`)

If the recipient is not connected when the message arrives:

1. The relay queues the message (`peer` as destination, sender pubkey, `nonce`, `ciphertext`,
   `created_at`).
2. If the recipient is a **client** with a `device_token`, the relay sends a **push**
   ([server.md §5](server.md)).
3. On the recipient's (re)connection, the relay drains the queue **in FIFO order** over the WS,
   then deletes delivered messages.
4. Queue TTL: **7 days**. Beyond that → silently dropped.

Queue limits per recipient: see §10.

### 6.4 Live Channel (`live=true`)

`live=true` selects a different delivery class:

| `live` | Relay semantics |
|--------|-----------------|
| `false` | **Store-and-forward**: if recipient is offline, queue (max 200, TTL 7d) and push. For approvals/clarifications. |
| `true` | **Route-or-fail**: forward **only** if the recipient is connected now. If offline → do NOT queue, do NOT push, reply to the sender with `PeerOffline { peer: <32B> }`. For state pulls and high-volume flows. Relay is **stateless** for this channel. |

On delivery the relay rewrites `Message.peer` from `to` (destination) to `from` (authenticated
sender), as in §6.2. `nonce`/`ciphertext` are never read.

### 6.5 Pull vs Notification: which traffic uses live (NORMATIVE)

The value of `live` is not a free choice: it depends on the **semantic nature** of the payload.

> **State pull → `live=true`. Event-driven notification that must wake the human → `live=false`
> (store-and-forward + push).**

A **pull** ("give me the current state") served stale is useless or harmful: route-or-fail is
correct — if the peer is absent, the sender knows immediately (`PeerOffline`) and shows an offline
state instead of hanging or receiving a stale snapshot hours later. A **notification** that must
reach an offline phone, however, *must* be able to wait in queue and be pushed.

| Payload | Direction | `live` | Why |
|---------|-----------|--------|-----|
| `inbox_request` (app open / reconnect) | client → agent | **`true`** | State pull: stale = useless. Agent offline → app learns immediately. |
| `inbox_update` in **response** to an `inbox_request` | agent → client | **`true`** | Client just asked: it is online by construction. |
| `inbox_update` for a **new event** (approval/clarification) | agent → client | **`false`** | Must reach an offline phone → queue + push. |

The sender, upon receiving `PeerOffline`, **stops** sending to that peer and retries on the next
`PresenceEvent { STATUS_ONLINE }` (§7) or on reconnect.

> **Why `PeerOffline` is needed even with presence.** Presence declares `ONLINE` with up to
> ~120 s delay on disconnect (idle-timeout). `PeerOffline` covers that blind window.
> Presence = *when to start*; `PeerOffline` = *correctness backstop*.

## 7. Presence

The relay exposes who is connected in the namespace. Scope is **strictly per namespace**: never
propagated outside. It only reveals pubkeys already known to the relay ([index.md §4.2](index.md)).

- `PresenceRequest {}` → relay replies `PresenceList { online: [<32B>, …] }` (snapshot, includes
  the requester).
- On `AuthOk` of a connection, **and** on its close (WS close or 120 s idle-timeout), the relay
  sends `PresenceEvent { pubkey: <32B>, status }` to **all other** connected namespace members.

Normative rules:
1. **Namespace scope**: no cross-namespace `PresenceEvent`.
2. **`OFFLINE` is best-effort and delayed** (up to ~120 s): not a guarantee of unreachability →
   the live channel has its own backstop `PeerOffline` (§6.4).
3. Idempotency: two consecutive `ONLINE` events for the same pubkey = no-op on the receiver.

## 8. Keepalive

- The relay sends **native WS ping frames** every **30 s**; the peer responds with a **pong frame**.
  These are native WebSocket opcodes, not protobuf messages.
- No traffic for **120 s** → the relay closes the connection.
- The agent reconnects with exponential backoff **1s, 2s, 4s, 8s, …, max 60s** (+ jitter).
- The client manages the WS according to its foreground/background lifecycle
  (see the iOS app repository documentation).

## 9. Pairing

Explicit process: the agent opens a window; the relay accepts `role: pairing` only during the
window; the token is **single-use**.

```
AGENT (perm. WS)             RELAY                   CLIENT (new WS)
  │ ─ PairingStart ──────────▶│                            │
  │   {token, ttl}            │                            │
  │ ◀─ PairingReady ──────────│                            │
  │  show QR ──────────────────────────────────────────── ▶│
  │                           │ ◀─ ws connect ─────────────│
  │                           │ ── Challenge ─────────────▶│
  │                           │ ◀─ Auth role:pairing ───────│
  │                           │    token, client pubkeys,  │
  │                           │    device_token, platform  │
  │                           │  verify: window? token ok? │
  │                           │  TTL? single-use? → consume│
  │                           │ ── AuthOk ────────────────▶│
  │ ◀─ ClientPaired ──────────│      (client → close WS)   │
  │   client pubkeys, plat.   │                            │
  │ ─ Authorize [.. new] ────▶│  (agent decides: authorise)│
  │ ◀─ AuthorizeOk ───────────│                            │
  │ ─ PairingStop ───────────▶│  close window              │
  │ ◀─ PairingStopOk ─────────│                            │
```

### 9.1 `PairingStart` (agent → relay)

```proto
PairingStart { pairing_token: <32B>, ttl: 300 }
```

- `pairing_token`: 32 random bytes (single-use bearer, [crypto.md §9](crypto.md)).
- `ttl`: seconds (default 300, max 600). The relay computes `expiry = now + ttl` and stores
  `{token, namespace_id, expiry, consumed: false}`.
- Response: `PairingReady { ttl: 300 }`.

If the agent calls `PairingStart` again while a window is open, the **new** token replaces the
previous one (the old token is immediately invalidated).

### 9.2 `PairingStop` (agent → relay)

```proto
PairingStop {}
```

Closes the window; the current token is invalidated. Response: `PairingStopOk {}`.

### 9.3 Implicit Stop

On `ttl` expiry without `PairingStop`, the relay closes the window automatically. A consumed
token stays consumed; an unused token becomes unusable.

### 9.4 `ClientPaired` (relay → agent)

```proto
ClientPaired {
  client_ed25519_pub: <32B>,
  client_x25519_pub:  <32B>,
  platform: PLATFORM_IOS | PLATFORM_ANDROID
}
```

The agent:
1. computes `shared_secret = X25519(agent_x25519_priv, client_x25519_pub)` and the `aes_key`
   ([crypto.md §4-5](crypto.md));
2. persists the client (pubkeys, counters at 0);
3. applies the **authorisation policy** (auto or user confirmation) and sends updated `Authorize`;
4. waits for the client's `hello` E2E message for detailed `device_info`.

## 10. Limits & Quotas (NORMATIVE, relay side)

| Limit | Value | Error |
|-------|-------|-------|
| Max frame size (pre-auth and store-and-forward) | 64 KiB | `payload_too_large` |
| Max frame size (live channel, post-auth) | 512 KiB | `payload_too_large` |
| Challenge timeout | 30 s | `challenge_timeout` |
| Idle timeout | 120 s | (silent close) |
| Store-and-forward queue TTL | 7 days | (silent drop) |
| Max queued messages per client | 200 | `queue_full` (rejects new until drained) |
| Max new connections per IP | 30 / minute | `rate_limited` |
| Max messages per connection | 60 / minute | `rate_limited` |
| Inactive namespace TTL | 7 days | (garbage collection) |
| Pairing `ttl` | default 300, max 600 s | (clamped) |

The 512 KiB limit for the live channel applies **only** to `RelayFrame { message { live: true } }`
and **only after `auth_ok`**. Any pre-auth frame over 64 KiB → `payload_too_large` (denies
unauthenticated flood amplification).

Values are reasonable defaults; the relay exposes them via config. Per-IP quotas contain
unauthenticated flood on the public endpoint.

## 11. Namespace Lifecycle

```
agent auth → namespace created (if new)
   ├── agent disconnected      → namespace "idle"
   ├── client connected        → namespace active
   ├── agent reconnected       → resumed
   └── 7 days without any connection → deleted (queues, tokens, authorised list, device_tokens)
```

Deletion is never explicit. If the agent reconnects after GC, the namespace is recreated from
scratch (same `namespace_id`, because it derives from the same key) but **without** any clients:
devices must re-pair.

## 12. Errors

Uniform format:

```proto
Error { code: "<code>", message: "<description>" }
```

`AuthError` uses the same shape, emitted during the handshake instead of `AuthOk`.

| Code | Meaning |
|------|---------|
| `challenge_timeout` | No `Auth` within 30 s. |
| `invalid_signature` | Challenge signature not valid. |
| `unauthorized` | Client not in authorised list. |
| `not_found` | Namespace or recipient not found / outside namespace. |
| `pairing_closed` | Namespace not in pairing mode, or token expired/consumed/wrong. |
| `rate_limited` | Per-IP or per-connection quota exceeded. |
| `payload_too_large` | Frame exceeds size limit. |
| `queue_full` | Recipient queue full. |
| `bad_request` | Malformed protobuf, missing field, wrong byte length. |

On all `auth_error` cases and after fatal errors, the relay closes the WS.

## 13. Summary: Everything on One WS

| Direction | Frames |
|-----------|--------|
| relay → anyone | `Challenge`, `AuthOk` / `AuthError`, `Message` (with `from`), `Error`, native WS ping |
| agent → relay | `Auth`(agent), `Authorize`, `PairingStart`, `PairingStop`, `Message`, native WS pong |
| relay → agent | `ClientPaired`, `AuthorizeOk`, `PairingReady`, `PairingStopOk`, `Message`, `PresenceEvent` |
| client → relay | `Auth`(pairing/client), `Message`, `PresenceRequest`, native WS pong |
| relay → client | `Message`, `PeerOffline`, `PresenceList`, `PresenceEvent` |

No REST endpoint exists. `namespace_id` is never in a query string.
