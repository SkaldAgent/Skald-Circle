# Mobile Connector Plugin (`mobile-connector`)

Bridges Skald's **Inbox** (approvals + clarifications + MCP elicitations) to mobile apps over the
**relay**, implementing the **agent** role of the v2 relay protocol. The plugin is
the namespace owner and the sole authority over authorized devices. Skald is
never exposed on the internet: only this plugin connects out, and only to the
relay.

- Crate: `crates/plugin-mobile-connector` — the **application** layer (thin).
- Networking: `crates/skald-relay-client` — the **standalone, payload-agnostic**
  relay client (WS v2 transport, E2E crypto, anti-replay counters, pairing,
  device authorization, SQLite persistence). It depends only on
  `skald-relay-common` (never on `core-api`), so it is reusable and unit/
  integration-tested in isolation. See [Crate split](#crate-split-skald-relay-client).
- Shared crypto + protobuf: `crates/skald-relay-common` (byte-for-byte interop
  with the reference vectors in [relay/test-vectors.md](../relay/test-vectors.md))
- **Protocol documentation** (canonical, in English):
  - [relay/index.md](../relay/index.md) — architecture, actors, threat model
  - [relay/relay-protocol.md](../relay/relay-protocol.md) — protobuf schema, auth, pairing, live channel, presence
  - [relay/framing.md](../relay/framing.md) — E2E plaintext framing (version + compression)
  - [relay/payloads.md](../relay/payloads.md) — JSON payload schemas (inbox_request, inbox_update, …)
  - [relay/crypto.md](../relay/crypto.md) — crypto contract, key derivation, AEAD, anti-replay

---

## Crate split: skald-relay-client

The plugin is the **application** layer; all networking lives in the standalone
`skald-relay-client` crate. The boundary is **payload-agnostic**: the client
exchanges opaque decrypted `Vec<u8>` payloads keyed by device pubkey and emits
inbound traffic as `RelayEvent`s; it never interprets the JSON. The plugin
consumes `client.events()`, applies the JSON schemas (`payloads.rs`) and the
`InboxApi`, and calls `client.send(dest, bytes, live)`.

Consequences of the split:

- **Authorization policy stays in the plugin.** On `RelayEvent::ClientPaired`,
  the client has already derived the `aes_key`, persisted the device as Pending,
  and consumed the pairing token. The plugin's event loop decides: if
  `require_device_confirmation` it notifies; otherwise it calls
  `client.authorize(ed)` and then `broadcast_inbox()`.
- **`client.authorize()` is payload-agnostic** — it does NOT push an Inbox
  snapshot. The plugin sends the snapshot after authorizing (both the auto path
  and the `RelayAgent::authorize_client` tool path).
- **v2 framing (`compress/decompress_payload`) is transport** — handled inside
  the client, so the plugin only ever sees clean JSON.
- **Identity seed** is injected via `SeedSource::Path("data/relay/seed")` (same
  relative path as before) so existing identities/devices survive the upgrade.

### Module map — `skald-relay-client` (networking)

| Module | Role |
|---|---|
| `config.rs` | `RelayClientConfig` + `SeedSource` (`Bytes` / `Path`) |
| `events.rs` | `RelayEvent` (`Connected`/`Disconnected`/`Message`/`ClientPaired`/`ClientRevoked`), broadcast |
| `identity.rs` | Seed load/generate (`0600`, injected path) + derived Ed25519/X25519 keys + `namespace_id` |
| `db.rs` | `relay_clients` table — devices + anti-replay counters (atomic counter helpers, `delete_all`) |
| `pairing.rs` | In-memory single-window pairing sessions (`code → session`) + `QrCodeData` |
| `state.rs` | Networking-only runtime: per-client `aes_key` cache, seal/open, counters, emits events |
| `ws.rs` | Permanent reconnecting agent WebSocket (v2 binary transport). Challenge → `Auth` → role dispatch → forward loop |
| `client.rs` | `RelayClient` — the public façade (`new`/`start`/`shutdown`/`send`/pairing/authorize/revoke/`clear_all`/`events`) |

### Module map — `plugin-mobile-connector` (application)

| Module | Role |
|---|---|
| `payloads.rs` | E2E JSON payload schemas (`inbox_update`, `notification`, client responses incl. `inbox_request`). Zlib-compressible per v2 framing.md |
| `app.rs` | `RelayApp`: Inbox dispatch (`broadcast_inbox`/`apply_client_payload`), authorization policy, the `events()` consumer loop |
| `notifier.rs` | `DelayedNotifier`: debounces phone pushes (`notify_delay_secs`) so resolving on the computer suppresses the push. See [Delayed push](#delayed-push) |
| `proxy.rs` | Accepts relay **pipes** of `stream_type = "http-local-proxy"` and reverse-proxies each to `127.0.0.1:<web_port>`. See [HTTP reverse proxy](#http-reverse-proxy-http-local-proxy) |
| `router.rs` | The QR-code HTTP endpoint (`/pairingqrcode`), resolves the current `RelayApp` → `client.lookup_pairing` |
| `agent.rs` | `RelayAgent` control trait (pairing, list, authorize, revoke) |
| `tools.rs` | The three LLM tools, registered in the main crate's `ToolRegistry` |
| `lib.rs` | `MobileConnectorPlugin` (`Plugin` + `RelayAgent`), lifecycle, bus subscriber, `RelayClient`/`RelayApp` wiring |

---

## Configuration

Stored in the `plugins` table (JSON, edited via the plugin UI / `configure_plugin`):

```yaml
relay_url: "wss://relay.skaldagent.net/v1/ws"  # empty ⇒ plugin idle (no WS)
pairing_ttl: 300                                # seconds, max 600
require_device_confirmation: true               # manual confirm new devices (recommended)
notify_delay_secs: 20                           # debounce before pushing to the phone (0 = immediate)
```

`enabled` (the standard plugin flag) starts/stops the runloop.

`notify_delay_secs` debounces the **phone push** for a new approval/clarification
(see [Delayed push](#delayed-push)). The mobile push is only useful when you're
away from the computer; if you answer at the computer within the window, no phone
notification is sent. Set `0` to push immediately.

---

## Persistence (plugin.md §9)

| Data | Location | Why |
|---|---|---|
| `seed` (32 B) | filesystem `data/relay/seed`, `0600` | the only persistent secret; keys + `namespace_id` are derived at runtime |
| Pairing session | **in-memory** only | transient (≤ TTL); lost on restart ⇒ just re-pair |
| Devices + `send/recv_counter` | DB `relay_clients` | **must** survive restarts |

### Why counters live in the DB

Skald self-restarts by design. If counters reset to 0 on restart:
- `send_counter → 0` reuses an AES-GCM nonce under the same key (breaks
  confidentiality + integrity for that device).
- `recv_counter → 0` re-opens the replay window.

So `send_counter` is incremented **and persisted before** sealing/sending
(`db::next_send_counter`, a transaction), and `recv_counter` is persisted only
**after** a valid `open`.

### `aes_key` cache

The per-client AES-256-GCM key is `HKDF(X25519(seed_x_priv, client_x_pub))`. It
is derived once and cached in memory (`HashMap<ed25519_pub, aes_key>` in
`RelayState`), never persisted; on a cache miss it is re-derived from the
client's stored `x25519_pub`. The cache entry is dropped on revoke.

---

## Pairing flow

1. The agent calls `mobile_start_pairing(ttl?)` (gated behind approval).
2. The plugin generates a 32-byte `pairing_token` (CSPRNG), sends
   `pairing_start{token, ttl}` to the relay, and registers an in-memory session
   keyed by a separate random `code` (latest-wins: any prior active session is
   marked *Superseded*). It returns the URL
   `/api/plugin/mobile-connector/pairingqrcode?code=<code>`.
3. The copilot renders the URL as an image. The endpoint serves a PNG of the QR
   while the session is **Active**, else a placeholder (`QR scaduto` /
   `QR già usato`). The QR payload is the normative `QrCodeData` JSON (never on
   disk, never in the URL).
4. The client scans, connects as `role:"pairing"`, the relay consumes the token
   and forwards `client_paired` to the agent.
5. On `client_paired`: derive + cache `aes_key`, persist the client as
   **Pending** (counters 0), mark the session **Consumed**, then apply the
   policy:
   - `require_device_confirmation = false` ⇒ auto-authorize.
   - `require_device_confirmation = true` ⇒ leave Pending; the human authorizes
     via the control surface (a `notification` is pushed to existing devices).

`authorize` always reflects the full local set (replacement semantics): adding a
device sends the complete list including it; revoking sends it without.

---

## Message flows

- **Inbox → clients:** the bus subscriber reacts to the six Inbox events
  (`approval_requested`, `approval_resolved`, `clarification_requested`,
  `clarification_resolved`, `elicitation_requested`, `elicitation_resolved`) and
  routes them through the **debouncer** (see
  [Delayed push](#delayed-push)) before building an `InboxSnapshot` via
  `inbox.list_pending()` and sending a sealed `inbox_update` to every Authorized
  client. Each approval
  carries a humanised `summary` (from `Tool::describe(Short)`, computed in
  `Inbox::list_pending`) for the card/notification plus the raw `arguments`
  (untruncated) for the detail dialog — so the user sees the full `execute_cmd`
  command, not a truncated label. Each clarification carries its
  `suggested_answers`. Each elicitation carries **only** its prompt metadata
  (`server_name`, `message`, `field_name`, `sensitive`, `is_confirmation`) — never
  a value; the value is supplied by the device in `elicitation_response.content`.
- **Clients → Inbox:** inbound `message` is checked (`from` ∈ Authorized,
  nonce direction + counter > `recv_counter`), opened, and dispatched by `kind`:
  `approval_response` → `inbox.approve/reject`, `clarification_response` →
  `inbox.answer`, `elicitation_response` → `inbox.resolve_elicitation` (its
  `content` may be a secret — never logged/persisted in clear), `hello` → persist
  `device_info`, `inbox_request` → send a **targeted** `inbox_update` back to
  `from` only (see below), `logout` → revoke.
  After any response the Inbox is re-snapshotted. `request_id` is mapped
  `string ↔ i64` (non-parsing ids are dropped). Inbox ops are idempotent by
  `request_id`.
- **Reconnect snapshot (`inbox_request`):** the relay does **not** notify the
  agent when a client reconnects, so the client sends `inbox_request` on the
  **live channel** (`Message.live=true`) after every `auth_ok` (e.g. when the app
  is opened from a push). The agent replies with an `inbox_update` sealed to the
  requester only — not a broadcast — so other devices are not needlessly
  re-aligned. A pull of stale state is useless, so the live channel is correct:
  if the agent is offline, the client gets `PeerOffline` immediately instead of
  waiting. Side-effect-free and idempotent (by `request_id`). See
  `data/ios-app/v2/relay-protocol.md` §3.1.

---

## HTTP reverse proxy (`http-local-proxy`)

So a remote device can reach Skald's web UI **without** a VPN/Tailscale or an open
port, the plugin reuses the relay **pipe** (relayed E2E byte-stream, see
[relay/pipe.md](../relay/pipe.md)) as a reverse proxy to the local HTTP server.

`proxy.rs` subscribes to `RelayClient::incoming_pipes()` and, for each invite with
`stream_type == "http-local-proxy"`, accepts the pipe and splices it byte-for-byte
to a **fresh** `TcpStream` to `127.0.0.1:<web_port>` (`PluginContext::web_port`).
Per pipe it `split`s the connection into independent send/receive halves and runs
each direction in its own task (full-duplex, so neither blocks the other;
`PipeSender::send` is backpressured by the pipe's ~10 MiB send buffer, and
`recv`/`read` are cancel-safe — see [relay/pipe.md §6.1](../relay/pipe.md#61-full-duplex--client-side-backpressure)).
When either direction ends it cancels a shared token so the other unwinds. Invites
of other `stream_type`s are **ignored** (not rejected) since `incoming_pipes` is a
broadcast shared with possible future consumers.

The native app side (later) opens one pipe per outbound connection and points a
WebView at it; because the tunnel is a transparent TCP splice, HTTP/1.1 keep-alive,
parallel connections, and the chat WebSocket upgrade all work unchanged.

**Security.**

- The destination is **pinned** to `127.0.0.1:<web_port>` — the client cannot pick
  host/port, so this is not an open localhost proxy (no SSRF to other local services).
- Access is gated by the relay's pipe auth (`pipe.md §3.1`): only the namespace
  agent or an **authorized** client can establish a pipe.
- It exposes the full local web UI remotely — that is the intent; pair/authorize
  devices accordingly.

**Relay tuning** (env, `pipe.md §3.3`): a browser opens several connections, so
`RELAY_PIPE_MAX_PER_NS` (default 8) may need raising; an idle chat-WS pipe can be
reaped at `RELAY_PIPE_IDLE_TIMEOUT_SECS` (120 s) — the frontend auto-reconnects.

Teardown: `proxy_one` takes a child of the plugin cancel token, so plugin stop
closes active tunnels; `stop_inner` also `shutdown()`s the relay client.

---

## Delayed push

A phone push is only valuable when the user is *away* from the computer. When
they're at the chat, every approval/clarification would otherwise fire an
instant — and pointless — notification, since they answer on the computer within
seconds. `DelayedNotifier` (`notifier.rs`) debounces this between the bus events
and `broadcast_inbox()`. (Elicitations are not chat-inline, so they are exempt —
see below.)

- **`*_requested`** arms a timer (`notify_delay_secs`, default 20s) keyed by
  `(kind, request_id)` — approvals, clarifications, and elicitations use
  independent id counters, so the kind is part of the key. If the timer elapses unresolved, the
  key is marked *notified* and the Inbox is pushed (`broadcast_inbox`, `live=false`
  → store-and-forward / offline push).
  - **Elicitations are the exception**: they live *only* in the Inbox (never
    inline in the chat, unlike approvals/clarifications), so there is no
    computer-side answer to debounce against. They skip the timer and are pushed
    **immediately**, regardless of `notify_delay_secs`.
- **`*_resolved`** before the timer fires ⇒ the timer is cancelled and **nothing
  is sent**. If the push already went out, the resolution is broadcast so the
  phone clears the item. (Untracked ids fall back to a broadcast for snapshot
  freshness.)

This only affects the **phone**: the desktop/web approval UI runs over the
per-session WebSocket (`ApprovalRequired`/`AgentQuestion`) and is never delayed.
Phone-driven responses (`apply_client_payload`) still `broadcast_inbox()`
immediately; the subsequent `*_resolved` bus event is handled idempotently. Armed
timers are cancelled on plugin stop (`cancel_all`). Set `notify_delay_secs: 0`
for the previous instant-push behaviour.

---

## LLM tools (plugin.md §11)

| Tool | Effect | Approval |
|---|---|---|
| `mobile_start_pairing(ttl?)` | Open the pairing window, return the QR URL | **Gated** (a default `require` rule is seeded, like `execute_cmd`/`restart`) |
| `mobile_list_devices()` | List devices (state, platform, device_info, last_seen) | read-only |
| `mobile_revoke_device(pubkey)` | Revoke a device by hex ed25519 pubkey | `Config` category |

These tools are not contributed through the `Plugin` trait (which has no
`tools()` method). They are registered in `Tools::build` (`src/core/skald/bundles.rs`):
the plugin is fetched via `get_plugin_typed::<MobileConnectorPlugin>()`, cast to
`Arc<dyn RelayAgent>`, and bound into the tools via
`plugin_mobile_connector::mobile_tools(agent)` → `ToolRegistry::register_arc`.

`mobile_start_pairing`'s approval gate is the default rule seeded in
`ApprovalManager::seed_defaults` (`src/core/approval/mod.rs`): opening a window
emits a secret (the QR) into chat, so it must be a deliberate human action, not
LLM-triggerable via prompt injection.

---

## HTTP endpoint

`GET /api/plugin/mobile-connector/pairingqrcode?code=<random>` — runtime PNG of
the QR (or placeholder), behind Skald's normal auth. Mounted by `WebFrontend`
via `Plugin::http_router()` (the router closes over the live `RelayState`). The
`code` is a non-enumerable capability; a URL leaked into `chat_history`
self-revokes once the window closes.
