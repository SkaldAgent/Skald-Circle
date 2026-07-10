# E2E Payloads — Encrypted Content Schemas

> This file defines the **plaintext** that is encrypted (AES-256-GCM, [crypto.md §6](crypto.md))
> and transported in the `ciphertext` field of the `Message` frame
> ([relay-protocol.md §6](relay-protocol.md)). **The relay never sees any of this.** Only the
> agent and the client see it.
>
> The plaintext is **JSON UTF-8**, wrapped in the framing envelope ([framing.md](framing.md))
> before encryption. No canonical form is required: it is encrypted as a byte blob and re-parsed
> by the recipient; it is never hashed separately.

---

## 1. Common Envelope

Every E2E payload has these base fields, plus kind-specific ones:

```json
{
  "v": 1,
  "kind": "<string>",
  "id": "<uuid-v4>",
  "ts": 1750000000000
}
```

| Field | Type | Required | Meaning |
|-------|------|----------|---------|
| `v` | int | yes | Payload schema version. `1` here. Different value → receiver discards with log. |
| `kind` | string | yes | Discriminant (table §2). |
| `id` | string (uuid-v4) | yes | Unique message id. Used for dedup at payload level and for acks. |
| `ts` | int (unix ms) | yes | Sender-side creation timestamp. Freshness check (§6). |

Common rules:

- **Forward-compat**: unknown fields are ignored. An unknown `kind` is discarded (with log),
  not a fatal error.
- **Idempotency**: the receiver MUST handle every payload idempotently relative to its action
  identifier (`request_id` for responses; `id` for generic dedup).
- **Anti-replay**: guaranteed by the nonce counter ([crypto.md §6.1](crypto.md)); `id`/`ts` are
  additional application-level defences.

---

## 2. Kind Catalogue

| `kind` | Direction | Purpose |
|--------|-----------|---------|
| `inbox_update` | agent → client | Full Inbox snapshot (pending approvals + clarifications + elicitations). |
| `notification` | agent → client | Generic notification (title/body), for informational pushes. |
| `hello` | client → agent | First message after pairing: detailed `device_info`. |
| `inbox_request` | client → agent | Explicit Inbox snapshot request; agent responds with a **targeted** `inbox_update`. |
| `approval_response` | client → agent | Outcome of an approval request. |
| `clarification_response` | client → agent | Answer to a clarification. |
| `elicitation_response` | client → agent | Reply to an MCP elicitation (carries the requested value E2E). |
| `logout` | client → agent | Device removes itself from the namespace. |
| `ack` | bidirectional | Delivery confirmation (optional, for reliability). |

---

## 3. Agent → Client

### 3.1 `inbox_update` — Inbox Snapshot

**Full snapshot**, not a delta: contains **all** currently pending items. Idempotent by
construction (replaces local state). So a lost push does not cause state loss: the next snapshot
realigns.

```json
{
  "v": 1,
  "kind": "inbox_update",
  "id": "0c5b…",
  "ts": 1750000000000,
  "badge": 2,
  "approvals": [
    {
      "request_id": "appr_8f2a…",
      "tool_name": "send_email",
      "agent_label": "Skald",
      "summary": "Send an email to mario@acme.com",
      "detail": "Subject: Q3 Estimate\nBody: …",
      "arguments": { "to": "mario@acme.com", "subject": "Q3 Estimate" },
      "created_at": 1749999990000
    }
  ],
  "clarifications": [
    {
      "request_id": "clar_3b1c…",
      "question": "Proceed with the €240 payment?",
      "context": "Invoice #1234, supplier X",
      "suggested_answers": ["Yes, proceed", "No, cancel"],
      "agent_label": "Skald",
      "created_at": 1749999991000
    }
  ],
  "elicitations": [
    {
      "request_id": "elic_5d7e…",
      "server_name": "ssh",
      "message": "Enter the SSH password for deploy@host",
      "field_name": "password",
      "sensitive": true,
      "is_confirmation": false,
      "created_at": 1749999992000
    }
  ]
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `badge` | int | yes | Total pending item count (= len(approvals)+len(clarifications)+len(elicitations)). Used by the client for badge. |
| `approvals[]` | array | yes | May be empty. |
| `approvals[].request_id` | string | yes | **Action identifier.** Stable while the item is pending. Used for response idempotency. |
| `approvals[].tool_name` | string | yes | Name of the tool requesting approval (e.g. `send_email`, `execute_cmd`). |
| `approvals[].agent_label` | string | yes | Human-readable origin label (typically `"Skald"`). |
| `approvals[].summary` | string | yes | Short line for notification/card (≤ ~120 chars). |
| `approvals[].detail` | string | no | Extended text for the detail screen. |
| `approvals[].arguments` | object | no | **Raw tool arguments** (JSON passed by the LLM). Source of truth for the detail screen: the client shows these so the user knows *what* they are approving (critical for `execute_cmd` → show `arguments.command`). May be absent for tools without arguments. E2E encrypted along with the rest of the payload. |
| `approvals[].created_at` | int (unix ms) | yes | When the request was created on the Skald side. |
| `clarifications[]` | array | yes | May be empty. |
| `clarifications[].request_id` | string | yes | Action identifier. |
| `clarifications[].question` | string | yes | Question to display. |
| `clarifications[].context` | string | no | Optional context. |
| `clarifications[].suggested_answers` | array of strings | no | Pre-defined answers suggested by the LLM. May be empty/absent. The client shows them as quick-tap options; free-form input is always possible too. The choice is sent as `clarification_response.answer` (§4.3). |
| `clarifications[].agent_label` | string | yes | Origin label. |
| `clarifications[].created_at` | int (unix ms) | yes | — |
| `elicitations[]` | array | yes | May be empty. MCP server-initiated input requests (e.g. an SSH/sudo password). |
| `elicitations[].request_id` | string | yes | Action identifier. Echoed back in `elicitation_response` (§4.4). |
| `elicitations[].server_name` | string | yes | MCP server that asked for input (e.g. `"ssh"`). |
| `elicitations[].message` | string | yes | Prompt to display to the user. |
| `elicitations[].field_name` | string \| null | no | Key the requested value must be stored under in `elicitation_response.content`. `null` for a bare confirmation. |
| `elicitations[].sensitive` | bool | yes | When `true`, the value is a secret: the client SHOULD mask input and MUST NOT cache/persist it. |
| `elicitations[].is_confirmation` | bool | yes | When `true`, this is a yes/no confirmation (no value field); `accept`/`decline` suffice and `content` is omitted. |
| `elicitations[].created_at` | int (unix ms) | yes | — |

> **Elicitation values never appear here.** This snapshot carries only the *prompt* metadata. The
> value the user supplies travels **only** in the client→agent `elicitation_response.content`
> (§4.4) and is handed straight to the MCP server; the agent never logs or persists it in clear.
>
> **Push privacy.** When this snapshot is sent to an offline client, the relay delivers it
> (encrypted) in the push *content-in-push* if it fits the APNs/FCM limit. Keep `summary`/`detail`
> short. If it exceeds the limit, the relay sends a *wake* and the client downloads the snapshot
> over WS ([server.md §5](server.md)).

### 3.2 `notification` — Generic Notification

```json
{ "v":1, "kind":"notification", "id":"…", "ts":…, "title":"Skald", "body":"Nightly job completed" }
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `title` | string | yes | Notification title. |
| `body` | string | yes | Notification body. |

No response required. Does not affect the badge unless accompanied by an `inbox_update`.

### 3.3 `ack` (optional)

```json
{ "v":1, "kind":"ack", "id":"…", "ts":…, "ref_id":"<id of confirmed payload>" }
```

Confirms that a payload with `id == ref_id` was received/processed. Optional (store-and-forward
+ idempotent snapshots suffice for v1).

---

## 4. Client → Agent

### 4.1 `hello` — Post-Pairing Application Handshake

First E2E message the client sends after it is authorised and connected as `client`. Transfers
detailed `device_info` **outside the relay's view**.

```json
{
  "v": 1,
  "kind": "hello",
  "id": "…",
  "ts": …,
  "device_info": {
    "platform": "ios",
    "model": "iPhone 16 Pro",
    "os_version": "18.5",
    "app_version": "1.0.0",
    "device_name": "Daniele's iPhone"
  }
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `device_info.platform` | string | yes | `"ios"` \| `"android"`. |
| `device_info.model` | string | no | Hardware model. |
| `device_info.os_version` | string | no | OS version. |
| `device_info.app_version` | string | no | App version. |
| `device_info.device_name` | string | no | Human-readable name for the agent's device list UI. |

The agent persists this data and shows it in the device list.

### 4.2 `approval_response` — Approval Outcome

```json
{
  "v": 1,
  "kind": "approval_response",
  "id": "…",
  "ts": …,
  "request_id": "appr_8f2a…",
  "decision": "approved",
  "reason": null,
  "bypass_secs": 900
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `request_id` | string | yes | MUST match an `approvals[].request_id` received. |
| `decision` | enum string | yes | **Only** `"approved"` \| `"rejected"`. Other values → agent discards. |
| `reason` | string \| null | no | Reason (typically for `rejected`). |
| `bypass_secs` | int | no | With `decision="approved"` only. Approve **and** register a bypass for similar tools: `900` = 15 minutes, `0` = for the entire session. **Absent** = single approval (current behaviour). The scope (tool category / MCP server / all) is auto-detected by the agent: the client only sends the seconds. |

Agent behaviour (see [../plugins/mobile-connector.md](../plugins/mobile-connector.md)):
1. Resolves the request via Skald's Inbox/ApprovalManager (`resolve(request_id, decision, reason)`).
   If `decision="approved"` and `bypass_secs` is present, uses `approve_with_bypass` instead
   of simple approve (registers the session bypass with auto-detected scope).
2. **Idempotency**: if `request_id` is already resolved (or no longer pending), the operation
   is a **no-op** (log and ignore). Neutralises replays and double deliveries.
3. Sends a new `inbox_update` (the snapshot will no longer contain that item) to realign clients.

### 4.3 `clarification_response` — Clarification Answer

```json
{
  "v": 1, "kind": "clarification_response", "id": "…", "ts": …,
  "request_id": "clar_3b1c…",
  "answer": "Yes, proceed."
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `request_id` | string | yes | MUST match a `clarifications[].request_id`. |
| `answer` | string | yes | Free-form answer text. |

Same `request_id` idempotency as §4.2.

### 4.4 `elicitation_response` — MCP Elicitation Reply

Reply to an `elicitations[]` entry (§3.1): an MCP server asked for an input the LLM must not see
(e.g. an SSH/sudo password). The requested value travels **only** in this payload's `content`,
sealed E2E — the relay never sees it and the agent hands it straight to the MCP server without
logging or persisting it.

```json
{
  "v": 1, "kind": "elicitation_response", "id": "…", "ts": …,
  "request_id": "elic_5d7e…",
  "action": "accept",
  "content": { "password": "hunter2" }
}
```

| Field | Type | Required | Notes |
|-------|------|----------|-------|
| `request_id` | string | yes | MUST match an `elicitations[].request_id`. |
| `action` | enum string | yes | **Only** `"accept"` \| `"decline"` \| `"cancel"`. Other values → agent discards. `decline` rejects the prompt; `cancel` aborts the whole request. |
| `content` | object \| null | conditional | Present **only** with `action="accept"` for a value prompt: a single key equal to the elicitation's `field_name`, whose value is the user's input (possibly a secret). Absent/null for `decline`/`cancel` and for confirmations (`is_confirmation=true`). A non-object `content` is dropped. |

Agent behaviour:
1. Resolves the request via Skald's Inbox (`resolve_elicitation(request_id, action, content)`), which
   forwards the outcome to the `ElicitationManager` and unblocks the waiting MCP call.
2. **Idempotency**: a `request_id` already resolved (or no longer pending) is a **no-op**.
3. Sends a new `inbox_update` to realign clients.

> **Secret hygiene.** `content` may carry a secret. It is never written to logs, the DB, or any
> trace on the agent side; it lives only long enough to satisfy the MCP `elicitation/create` call.

### 4.5 `logout` — Device Self-Removal

```json
{ "v":1, "kind":"logout", "id":"…", "ts":… }
```

The agent, on receipt:
1. removes `client_ed25519_pub` from the local authorised list;
2. sends an updated `Authorize` (without that client) to the relay → the relay closes the
   device's WS, purges its queue, and forgets its `device_token`
   ([relay-protocol.md §5](relay-protocol.md));
3. forgets the client's keys/counters.

> Revocation can also be initiated **by the agent** (lost/stolen device): the user removes it via
> the Skald UI and the agent sends `Authorize` without that device. `logout` E2E is only the
> "device-initiated" case.

### 4.6 `inbox_request` — Explicit Inbox Snapshot Request

The client sends this payload to ask the agent for the current Inbox state.
**MUST be sent after `AuthOk` on every WS (re)connection** (including app open from a push),
because the agent does **not** receive a reconnect signal from the relay: without `inbox_request`
the client's Inbox would stay empty until a new bus event triggers a broadcast.

```json
{ "v":1, "kind":"inbox_request", "id":"…", "ts":… }
```

No specific fields beyond the common envelope (§1).

Agent behaviour:
1. Builds the current Inbox snapshot (`list_pending()`).
2. Sends an `inbox_update` (§3.1) **targeted to the requester only** (not a broadcast): the
   message is sealed with the requesting client's `aes_key`, leaving other devices unaffected.
3. Idempotent and side-effect-free on the Inbox: safe to send on every connection. If there are
   no pending items, the snapshot has `badge:0` and empty arrays.

> This follows the *targeted request → targeted response* pattern. The payload travels on the
> **live channel** (`Message.live=true`, [relay-protocol.md §6.4](relay-protocol.md)): a stale
> Inbox snapshot is useless, so route-or-fail is correct — if the agent is offline, the client
> learns immediately via `PeerOffline`.

### 4.7 `ack` (optional)

Same as §3.3, opposite direction.

---

## 5. Inbox State Machine (client side)

```
            inbox_update (snapshot)
   ┌──────────────────────────────────────┐
   ▼                                        │
[ local list ] ──user approves/rejects──▶ send approval_response
   ▲                                        │  (optimistic: remove card)
   │                                        ▼
   └──────────── next inbox_update ◀─── agent resolves and re-snapshots
```

- The client updates the UI **optimistically** (removes the card on response send), but the
  **source of truth** is the next `inbox_update`. If the response is lost, the item reappears
  on the next snapshot.
- Local `badge` = `badge` of the last snapshot, minus items already responded to locally
  (reconciled on next snapshot).

## 6. Freshness & Validation (receiver side)

For every decrypted E2E payload, the receiver MUST:

1. verify the nonce **counter** (`> last_seen`, [crypto.md §6.1](crypto.md)) → otherwise discard;
2. verify `v == 1` → otherwise discard with log;
3. (SHOULD) discard if `|now - ts|` > 7 days (aligned with the queue TTL): extra defence against
   very late replays;
4. validate required fields and types; a malformed payload is discarded without crash;
5. apply the action **idempotently** by `request_id` (responses) or `id` (generic dedup).

## 7. Complete Round-Trip Examples

**Approval (foreground):**
```
agent  → inbox_update { approvals:[{request_id:"appr_1", tool_name:"send_email", …}], badge:1 }
client → approval_response { request_id:"appr_1", decision:"approved" }
agent  → inbox_update { approvals:[], badge:0 }     // realign
```

**Clarification (background, via content-in-push):**
```
agent  → inbox_update { clarifications:[{request_id:"clar_9", question:"Proceed?"}], badge:1 }
         (relay: client offline → push with encrypted blob)
client → (NSE decrypts, shows notification) → user opens app → clarification_response { request_id:"clar_9", answer:"Yes" }
agent  → inbox_update { clarifications:[], badge:0 }
```

**MCP elicitation (SSH password, secret E2E):**
```
         (MCP `ssh` server calls elicitation/create → agent blocks the tool call)
agent  → inbox_update { elicitations:[{request_id:"elic_5", server_name:"ssh", field_name:"password", sensitive:true}], badge:1 }
client → (masked input) → elicitation_response { request_id:"elic_5", action:"accept", content:{ "password":"hunter2" } }
agent  → (hands content to the MCP server, unblocks the call; value never logged) → inbox_update { elicitations:[], badge:0 }
```

**App opened from notification (reconnect):**
```
client → (connects as role:"client", auth_ok)
client → inbox_request { }                  // live channel (Message.live=true)
agent  → inbox_update { approvals:[…], clarifications:[…], badge:N }   // targeted to requester only
```
