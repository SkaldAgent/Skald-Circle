# Relay Server — Implementation

> Guide for the coding agent building `crates/skald-relay-server`. The **protocol** is in
> [relay-protocol.md](relay-protocol.md); the **cryptography** (which the relay barely touches)
> is in [crypto.md](crypto.md). Here: internal architecture, persistence, push bridge, deploy,
> quotas.

---

## 1. Role (and Non-Role)

The relay is the **only centralised component**. It does **only** four things:

1. **Authenticates** connections (Ed25519 challenge-response) and routes by `namespace_id`.
2. **Forwards** opaque messages between agent and clients of the same namespace.
3. **Store-and-forward**: queues for offline recipients.
4. **Push bridge**: for offline clients it talks to APNs (Apple) and FCM (Google).

It does **nothing else**: no business logic, no decryption, no content reading, no user accounts.
Deliberately dumb. Its only "truth" is: `pubkey → namespace`, `pubkey → device_token`, and a
FIFO queue of blobs.

### Zero-Trust: What It Means Here (precise)

The relay is **content-confidential**, **not** metadata-private (see [index.md §4](index.md)).
It sees pubkeys, device_tokens, IPs, the relationship graph, and timing; it does **not** see
content or detailed `device_info` (which travel E2E). Everything the relay persists is either
non-sensitive or E2E-encrypted.

---

## 2. Stack & Structure

Language: **Rust**. Static musl binary ~5–7 MB, ~30 MB RAM, cold start < 100 ms.

| Crate | Use |
|-------|-----|
| `axum` | HTTP server + WebSocket upgrade, healthcheck |
| `tokio` / `tokio-tungstenite` | async runtime + per-connection WS |
| `prost` | protobuf encode/decode (`RelayFrame` from `skald-relay-common`) |
| `sqlx` (sqlite) | persistence (namespaces, clients, queue) |
| `ed25519-dalek` = "2" | challenge-response signature verification |
| `sha2`, `hex` | hashing and encoding |
| `a2` | APNs HTTP/2 + JWT |
| `reqwest` | FCM HTTP v1 (Android) |
| `tracing` | structured logs (metrics only, **never** content) |
| `clap` | CLI flags |
| `governor` | per-IP / per-connection rate limiting |

```
crates/skald-relay-server/
├── Cargo.toml
└── src/
    ├── main.rs        # config, init, axum server, graceful shutdown
    ├── ws.rs          # WS handler: challenge → auth(role) → forward loop
    ├── auth.rs        # signature verification, namespace_id derivation, role gating
    ├── routing.rs     # live connection registry (namespace → agent/clients)
    ├── store.rs       # sqlx: namespaces, clients, queue, pairing
    ├── push.rs        # APNs + FCM bridge, content-in-push vs wake
    ├── limits.rs      # quotas, rate-limit, timeouts
    └── types.rs       # serde types for JSON (used only for push payloads / logging)
```

> **Shared crate.** Frame types and auth crypto (signature verification + `namespace_id`
> derivation) are **common** with the plugin and live in `crates/skald-relay-common`.
> The relay depends on that crate. X25519/HKDF/AES-GCM remain in the shared crate for E2E
> (plugin + app) and for the `gen-vectors` binary, not used by the relay itself.

---

## 3. Data Model (SQLite)

Minimal schema. **No sensitive data in plaintext**: `ciphertext` is E2E; pubkeys are public
identifiers.

```sql
CREATE TABLE namespaces (
    namespace_id      TEXT PRIMARY KEY,          -- hex(SHA256(domain‖pub)), immutable
    agent_ed25519_pub BLOB NOT NULL UNIQUE,      -- 32B, binds id to key
    created_at        INTEGER NOT NULL,          -- unix ms
    last_active       INTEGER NOT NULL,          -- for 7-day GC
    -- pairing window (at most one active per namespace):
    pairing_token     BLOB,                      -- 32B random, NULL if closed
    pairing_expiry    INTEGER,                   -- unix ms
    pairing_consumed  INTEGER NOT NULL DEFAULT 0 -- 0/1 single-use
);

CREATE TABLE clients (
    namespace_id       TEXT NOT NULL REFERENCES namespaces(namespace_id) ON DELETE CASCADE,
    client_ed25519_pub BLOB NOT NULL,            -- 32B, routing + auth
    client_x25519_pub  BLOB NOT NULL,            -- 32B, opaque (forwarded to agent)
    device_token       TEXT,                     -- push token (APNs/FCM)
    platform           TEXT NOT NULL,            -- 'ios' | 'android'
    state              TEXT NOT NULL,            -- 'pending' | 'authorized'
    last_seen          INTEGER,                  -- unix ms
    PRIMARY KEY (namespace_id, client_ed25519_pub)
);

CREATE TABLE queue (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    namespace_id  TEXT NOT NULL REFERENCES namespaces(namespace_id) ON DELETE CASCADE,
    to_pub        BLOB NOT NULL,                 -- 32B recipient
    from_pub      BLOB NOT NULL,                 -- 32B sender (guaranteed by relay)
    nonce         BLOB NOT NULL,                 -- 12B
    ciphertext    BLOB NOT NULL,                 -- opaque (ciphertext‖tag)
    created_at    INTEGER NOT NULL               -- unix ms (for 7-day TTL)
);
CREATE INDEX idx_queue_dest ON queue(namespace_id, to_pub, id);
```

Notes:
- `client_x25519_pub` is persisted for **robustness** (re-forwarding `ClientPaired` if the agent
  missed it), even though the relay does not use it for crypto.
- The relay does NOT store `shared_secret`/`aes_key` (it neither has them nor can compute them).
- `state='pending'` until the agent sends `Authorize` including the pubkey (→ `authorized`).
  A `role:"client"` connection is only accepted from `authorized`.

### ⚠️ Constraint: SQLite on EFS ⇒ Single Instance

In v1 the relay runs as a **single Fargate task** with SQLite on an EFS volume. SQLite on NFS/EFS
**does not support** concurrent writes from multiple processes (unreliable locking → corruption).
Therefore:

- **Do NOT** scale horizontally with this configuration (no HA, no multi-task).
- Store-and-forward assumes a **single writer**.
- **Scale path** (post-v1, if HA is needed): replace `store.rs` with Postgres (RDS) for the
  queue and a distributed connection registry (e.g. Redis pub/sub) for cross-instance routing.
  The `store.rs` API is designed to make this substitution localised.

---

## 4. Concurrency & Routing

Model: **one Tokio task per WS**.

- `routing.rs` holds in memory `DashMap<namespace_id, NamespaceConns>` where
  `NamespaceConns { agent: Option<Sender>, clients: HashMap<pubkey, Sender> }` and `Sender` is
  a `tokio::sync::mpsc::Sender<RelayFrame>` toward that WS's task.
- **Forwarding**: on receiving a `Message` from an authenticated WS, check `peer` (destination):
  - if the recipient has a live connection in the same namespace → send on its `Sender`;
  - otherwise → `store::enqueue(...)` and, for a client, `push::notify(...)`. Unless
    `Message.live=true`, in which case → send `PeerOffline { peer }` back to the sender.
- **Single agent**: one `agent` connection per namespace; a new one displaces the old (close old).
- **Single client per pubkey**: same for devices.
- **Keepalive**: ping task every 30 s; close on 120 s silence
  ([relay-protocol.md §8](relay-protocol.md)).

### Store-and-Forward (delivery)

```rust
async fn deliver_pending(tx: &Sender<RelayFrame>, store: &Store,
                         ns: &str, to_pub: &[u8;32]) -> anyhow::Result<()> {
    for m in store.fetch_pending(ns, to_pub).await? {     // ORDER BY id ASC (FIFO)
        tx.send(build_message_frame(m.from_pub, m.nonce, m.ciphertext, false)).await?;
        store.delete_pending(m.id).await?;                // delete after delivery
    }
    Ok(())
}
```

Queue full (> 200 for recipient, [relay-protocol.md §10](relay-protocol.md)) → reject new
messages with `queue_full` until drained. TTL: a periodic task deletes messages older than 7 days
and namespaces inactive for 7 days.

---

## 5. Push (APNs / FCM Bridge)

When a message is destined for an **offline client** with a `device_token`, the relay sends a
push. Two modes, decided by the **size of the encrypted blob**:

### 5.1 Content-in-Push (preferred, enables "approve from notification")

If `len(raw ciphertext)` fits within the payload limit (**APNs ~4 KiB**, **FCM ~4 KiB**), the
relay includes the **already E2E-encrypted blob** in the push. The device decrypts it in the
Notification Service Extension and shows a rich notification with Approve/Reject actions,
**without** opening the app.

**APNs payload**:
```json
{
  "aps": {
    "alert": { "title": "Skald", "body": "Action required" },
    "badge": 1,
    "sound": "default",
    "mutable-content": 1,
    "category": "skald_inbox"
  },
  "d": {
    "ns": "<namespace_id hex>",
    "from": "<agent_ed25519_pub hex>",
    "n": "<nonce hex 24>",
    "c": "<ciphertext base64>"
  }
}
```

- `mutable-content: 1` activates the Notification Service Extension (decrypts `d.c`).
- `aps.alert` is a **generic fallback** shown if the NSE fails: **never** sensitive content.
- The relay does NOT know what is in `d.c`: it copies it as-is from the queue.

> Note: inside `d`, the values use hex/base64 encoding for JSON compatibility (nonce hex 24 chars,
> ciphertext base64). This is the one context where the encoding conventions from
> [index.md §5](index.md) apply outside the E2E JSON payload.

### 5.2 Wake-Only (fallback when blob exceeds limit)

```json
{
  "aps": { "alert": { "title":"Skald", "body":"Action required" },
           "badge":1, "sound":"default", "content-available":1 },
  "d": { "ns": "<namespace_id hex>", "wake": true }
}
```

The device wakes, opens a **temporary WS**, downloads queued messages, and shows the Inbox.
No content in the push.

> **Choice rule (normative):** content-in-push if `len(raw_ciphertext_bytes) <= 3500` (after
> base64-encoding into JSON it will be ≤ ~4666 chars), otherwise wake-only. Conservative threshold
> to leave room for other fields. Keep `summary`/`detail` in payloads small to stay in the
> preferred case.

### 5.3 FCM (Android)

Use **FCM HTTP v1** with a **data-only message** (`"data": { … }`) so the app always handles
decryption (even in background), avoiding automatic display of an unencrypted `notification`.
Fields `ns`/`from`/`n`/`c` as above. Priority `high`.

### 5.4 Push Key Management

| Secret | Where | How |
|--------|-------|-----|
| APNs `.p8` (Apple) | **relay only** | AWS Secrets Manager in prod; `config/apns-key.json` (git-ignored) locally |
| FCM service account JSON (Google) | **relay only** | Secrets Manager / local file |

Never in the app, never in the plugin. At startup the relay loads secrets and generates:
- **APNs JWT** (ES256, valid 60 min) held in memory, **refreshed every ~30 min** (never more
  than once every 20 min, per Apple's rules). No key on disk beyond the `.p8`.
- **FCM OAuth token** (from the service account) with auto-refresh.

Example APNs secret in Secrets Manager:
```json
{ "team_id":"ABC123DEFG", "key_id":"XYZ789ABCD",
  "private_key":"-----BEGIN PRIVATE KEY-----\nMIGTA…\n-----END PRIVATE KEY-----" }
```

Minimal IAM for the ECS task:
```json
{ "Effect":"Allow", "Action":"secretsmanager:GetSecretValue",
  "Resource":"arn:aws:secretsmanager:REGION:ACCOUNT:secret:skald/push-keys-*" }
```

---

## 6. Security Checklist

- [ ] **WSS mandatory**: reject plain `ws://`.
- [ ] Verify Ed25519 signature **before** any other logic; reject malformed input with `bad_request`.
- [ ] `namespace_id` recomputed from pubkey, **never** trusted from client input.
- [ ] Pairing token and tag comparisons in **constant-time** (`subtle`).
- [ ] `PairingStart` token **single-use** enforced atomically (`UPDATE … WHERE consumed=0`).
- [ ] Role gating: `client` only if `authorized`; `pairing` only if window open + token valid.
- [ ] **Rate-limit** per-IP on new connections and per-connection on messages (`governor`).
- [ ] Frame size limit 64 KiB (pre-auth + store-and-forward), 512 KiB (live channel post-auth);
      `payload_too_large` + close on exceeded.
- [ ] **No content in logs**: log only `namespace_id`, truncated pubkeys, codes, counts, latencies.
      **Never** `ciphertext`, `nonce`, full `device_token` (truncate/hash).
- [ ] `Authorize` shrink → close the revoked client's WS + **purge their queue** + forget `device_token`.
- [ ] 7-day GC for namespaces/queues.

---

## 7. Startup & Shutdown

1. `main.rs` loads config (CLI/env): port, DB path, push key source, thresholds.
2. Load push keys (Secrets Manager or file). Generate APNs JWT + FCM token (refresh task).
3. Initialise SQLite (migrations via `sqlx::migrate!`).
4. Start axum on `0.0.0.0:{port}`; route `GET /healthz` → 200; `GET /v1/ws` → upgrade.
5. **Graceful shutdown** on SIGTERM/SIGINT: stop accepting, drain WS connections, flush queue,
   close DB.

### Logging

`main.rs` writes logs to both **stdout** and a file at **`logs/skald-relay.log`** (daily rotation
via `tracing-appender`), aligned with the main app. Log level controlled by `RUST_LOG`; default
`skald_relay_server=info,info`. In development:

```sh
RUST_LOG=skald_relay_server=debug   # auth, routing, queue drain
RUST_LOG=skald_relay_server=trace   # frame-level tracing
```

Invariant: **never log content** — only `namespace_id`, truncated pubkeys, codes, counts (see §6).

---

## 8. Deploy

| Aspect | v1 choice |
|--------|-----------|
| Compute | AWS ECS **Fargate**, **1 task** (§3 constraint) |
| Container | musl static, `FROM scratch`, ~7 MB |
| Storage | SQLite on **EFS** (persistent across restarts) |
| Push keys | Secrets Manager (`skald/push-keys`) |
| Domain/TLS | `relay.skaldagent.net` via ALB + ACM (free TLS) |
| Logs | CloudWatch (metrics only) |
| Cost | ~$5–10/month Fargate + ~$0.40 Secrets Manager |

```dockerfile
FROM clux/muslrust:stable AS build
COPY . /src
WORKDIR /src
RUN cargo build --release --target x86_64-unknown-linux-musl -p skald-relay-server --bin skald-relay-server

FROM scratch
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/skald-relay-server /skald-relay-server
EXPOSE 8080
ENTRYPOINT ["/skald-relay-server"]
```

### Self-Hosting

Anyone can host their own relay: an Apple Developer Key ($99/year) for APNs (and/or a Firebase
project for FCM) is required. `docker compose` with local SQLite, or deploy on your own cloud.
The relay is open source and interoperable with any agent/app conforming to these documents.

---

## 9. Definition of Done

- [ ] `cargo build --release` produces a musl static binary.
- [ ] An agent can authenticate, create the namespace, start/stop pairing, authorize.
- [ ] A client can pair, then connect as `client` only after `authorize`.
- [ ] Messages routed live; store-and-forward + FIFO delivery on reconnect.
- [ ] Push content-in-push below threshold, wake-only above; APNs and (at least stub) FCM working.
- [ ] Revocation via `Authorize` shrink closes WS and purges queue.
- [ ] Live channel: `PeerOffline` sent correctly; no queue/push for `live=true` messages.
- [ ] Presence: `PresenceList` on `PresenceRequest`; `PresenceEvent` on connect/disconnect.
- [ ] Quotas/rate-limit active; no content in logs.
- [ ] 7-day GC verified.
- [ ] Relay never alters `nonce`/`ciphertext` (test: bytes identical in/out).
