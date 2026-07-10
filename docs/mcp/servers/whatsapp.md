# WhatsApp MCP Server (whatsapp)

## Overview

A Node.js MCP server that exposes WhatsApp as a set of tools for the LLM, using **whatsapp-web.js** + Puppeteer (headless Chromium).

**Server name:** `whatsapp`  
**Transport:** `stdio` (spawns `node scripts/whatsapp_mcp/index.js`)  
**Location:** `scripts/whatsapp_mcp/index.js`

### Capabilities

| Capability | Enabled |
|------------|---------|
| List chats and groups | ✅ |
| Read messages from a chat | ✅ |
| Search messages by keyword | ✅ |
| Search contacts by name | ✅ |
| Send messages | ✅ |
| Send media (image/video/audio/document, from file or URL) | ✅ |
| Download received media (photos, videos, documents) | ✅ |
| Logout / reset session without restart | ✅ |
| Address a contact by phone number (no lookup needed) | ✅ |
| Edit/delete messages | ❌ not implemented |

---

## ⚠️ Important notes on WhatsApp

- **whatsapp-web.js is unofficial**: it drives WhatsApp Web through Chromium. WhatsApp may ban the number in case of heavy or anomalous use.
- **Safe use**: reading your own groups and sending individual messages is in the tolerated grey area. Do not use it for spam or bulk automation.
- **Recommended number**: using a secondary number or a parallel WhatsApp Business account reduces the risk.

---

## Tools

All tools are callable as `mcp__whatsapp__<tool>`; the table lists the bare `<tool>` names.

| Tool | Parameters | Description |
|------|------------|-------------|
| `status` | *(none)* | Connection status as a plain-language report: the state, what it means, and step-by-step fix instructions when not operational. Cross-checks the live socket (`getState()`) to catch silently dropped sessions |
| `get_qr` | *(none)* | QR code to scan with the phone — path/URL to a PNG or HTML page, with ASCII fallback (only when status = QR_READY) |
| `logout` | *(none)* | Ends the session, **clears the cached credentials on disk** and re-initializes the client → new QR without restart. Use it when the session has expired/got stuck or to link a different phone |
| `list_chats` | `max_chats` (int, default 20, max 50) | List recent chats with name, ID and unread count |
| `get_messages` | `chat_id` **or** `number`, `limit` (int, default 20, max 100), `offset` (int, default 0) | Messages from a chat/group with pagination support. Media messages are tagged with their type and a `download id` |
| `send_message` | `chat_id` **or** `number`, `message` (required) | Send a text message |
| `send_media` | `chat_id` **or** `number`, `source` (required: file path or http(s) URL), `caption`, `as_document` (bool) | Send an image/video/audio/document |
| `download_media` | `message_id` (required) | Download the media attached to a message; saves it under `data/whatsapp_media/` and returns the path + `/data/` URL |
| `search_messages` | `query` (required), `max_results` (int, default 20, max 50) | Search by keyword across all chats |
| `search_contacts` | `query` (required), `max_results` (int, default 20, max 50) | Search saved contacts by name (partial, case-insensitive). Use it to find the ID of a contact not present in recent chats |

### chat_id format

- **Contact:** `39xxxxxxxxxx@c.us` (international prefix without `+`, followed by `@c.us`)
- **Group:** `xxxxxxxxxx-xxxxxxxxxx@g.us`

Correct chat_ids are obtained via `list_chats` (recent chats) or `search_contacts` (saved contacts not in recent chats).

**Shortcut for individual contacts:** `get_messages`, `send_message` and `send_media` also accept a plain `number` (phone number with country code, e.g. `393331234567` or `+39 333 123 4567`) instead of a `chat_id`. The server resolves it via `getNumberId` (which also verifies the number is on WhatsApp), so there is no need to look up the chat_id first. Groups must still be addressed by `chat_id`.

---

## Authentication

### First time (QR scan)

On the first launch there is no saved session. The client generates a QR code:

1. The LLM calls `status` → response `QR_READY`
2. The LLM calls `get_qr` → returns the QR
3. The user scans the QR with WhatsApp → **Settings → Linked Devices → Link a Device**
4. The state moves to `AUTHENTICATED`, then `READY`

The QR is also saved to a file (PNG at `data/whatsapp_qr.png`, HTML at `secrets/whatsapp_qr.html`, ASCII fallback at `secrets/whatsapp_qr.txt`).

### Subsequent sessions

The session is persisted in `secrets/whatsapp_session/` (managed by whatsapp-web.js's `LocalAuth`). On server restart the session is restored automatically, with no need to scan the QR again.

### Browser lifecycle & restart cleanup (self-healing)

The Puppeteer profile lives at `secrets/whatsapp_session/session` and Chromium takes an **exclusive lock** on it (`SingletonLock`): two browsers cannot open the same profile at once.

The host (skald) kills MCP servers with **SIGKILL** on shutdown/restart (`kill_on_drop` in the Rust MCP client — see `crates/mcp-client/src/server.rs`). SIGKILL is untrappable, so the node process dies instantly and the headless Chromium it spawned is **orphaned** (reparented to init), keeping the profile locked. The next launch would then fail with *"The browser is already running for &lt;profile&gt;. Use a different `userDataDir` or stop the running browser first."* — and every WhatsApp tool stays dead until the orphan is killed by hand.

To make this self-healing, on **every startup** (in `initClient()` → `cleanupStaleBrowser()`) the server:

1. Runs `pgrep -f <profile-dir>` and `SIGKILL`s every process still bound to the profile. Because skald runs a single WhatsApp MCP at a time and SIGKILLs the old node before spawning the new one, any such process is guaranteed to be a leftover orphan.
2. Removes the stale `SingletonLock` / `SingletonCookie` / `SingletonSocket` files so a fresh launch is unblocked.

For the shutdown paths that *are* trappable (clean stdin EOF, `SIGTERM`/`SIGINT` from a container/OS stop), the server closes the browser cleanly (`client.destroy()`, awaited with a 5 s cap) so it releases the lock on the way out. The startup cleanup is the backstop that covers the SIGKILL path. The saved login (under `session/Default/`) is untouched, so no QR re-scan is needed after a restart.

### Logout / expired session (without restart)

When the session expires or gets stuck (state `DISCONNECTED`), on restart `LocalAuth` would reload the invalid session from `secrets/whatsapp_session/`, immediately returning to the disconnected state. Previously the only fix was to delete that folder by hand and restart.

Now `logout` is enough:

1. Attempts a clean logout (`client.logout()`), tolerating failure if the browser page is already dead;
2. As a fallback, closes the browser (`destroy()`) to release the locks on the profile;
3. **Force-deletes `secrets/whatsapp_session/`** (the cached token);
4. Removes any stale QR files;
5. Re-initializes the client → generates a new QR within a few seconds.

```
mcp__whatsapp__logout()
# → wait a few seconds
mcp__whatsapp__get_qr()
# → scan the new QR
```

No server restart required.

### Token storage

| File/Directory | Contents |
|---|---|
| `secrets/whatsapp_session/` | Persistent WhatsApp session (LocalAuth) — deleted by `logout` |
| `secrets/whatsapp_qr.html` / `data/whatsapp_qr.png` | Temporary QR code (removed after authentication or a logout) |
| `secrets/whatsapp_qr.txt` | ASCII fallback of the QR (when `qrcode` is unavailable) |
| `data/whatsapp_media/` | Media downloaded via `download_media`, served at `/data/whatsapp_media/` |

Everything under `secrets/` is in `.gitignore` via the `secrets/` rule.

---

## Setup (one-time)

### 1. Install the Node.js dependencies

```bash
cd scripts/whatsapp_mcp
npm install
```

This installs `whatsapp-web.js`, `puppeteer` (includes Chromium, ~300MB), `qrcode` and `qrcode-terminal`.

### 2. Register the server (have the agent do it)

```
register_mcp(
  name="whatsapp",
  transport="stdio",
  command="node",
  args=["scripts/whatsapp_mcp/index.js"]
)
```

### 3. First authentication

```
mcp__whatsapp__status()
# → QR_READY

mcp__whatsapp__get_qr()
# → shows the QR, scan it with the phone
```

---

## Usage examples

### See recent chats

```
mcp__whatsapp__list_chats(max_chats=10)
```

### Read the latest messages from a group

```
mcp__whatsapp__get_messages(
  chat_id="1234567890-9876543210@g.us",
  limit=50
)
```

### Page through history (older messages)

`offset` skips the most recent messages, exposing the preceding window:

```
# Last 20 messages
get_messages(chat_id="...", limit=20, offset=0)

# Messages 21–40 (previous)
get_messages(chat_id="...", limit=20, offset=20)

# Messages 41–60 (even older)
get_messages(chat_id="...", limit=20, offset=40)
```

Limit: `limit + offset` cannot exceed 200 in a single call (a `fetchMessages` constraint).

### Find the contact of someone not in recent chats

```
mcp__whatsapp__search_contacts(query="Luca")
# → Luca Rossi [contact] | ID: 393331234567@c.us
```

### Search for what was said about a topic

```
mcp__whatsapp__search_messages(query="Monday meeting")
```

### Send a message

```
# By chat_id (groups, or chats already open)
mcp__whatsapp__send_message(
  chat_id="393331234567@c.us",
  message="Hi! Are you there?"
)

# Or directly by number — no list_chats/search_contacts needed first
mcp__whatsapp__send_message(
  number="+39 333 123 4567",
  message="Hi! Are you there?"
)
```

### Send media (image, video, document)

```
# From a local file (path relative to the project root, or absolute)
mcp__whatsapp__send_media(
  number="393331234567",
  source="data/report.pdf",
  caption="Here is the report",
  as_document=true
)

# From a URL
mcp__whatsapp__send_media(
  chat_id="1234567890-9876543210@g.us",
  source="https://example.com/photo.jpg",
  caption="Look at this"
)
```

### Download received media

`get_messages` tags media messages with a `download id`:

```
mcp__whatsapp__get_messages(number="393331234567", limit=10)
# → [2026-06-22 10:01:00] Luca [image, download id="true_39...@c.us_3EB0..."]: invoice photo

mcp__whatsapp__download_media(message_id="true_39...@c.us_3EB0...")
# → saved to data/whatsapp_media/...  (also served at /data/whatsapp_media/...)
```

---

## Connection states

`status` returns a self-describing report — it states the lifecycle state, explains it in plain language, and lists concrete next steps whenever it is not `READY`. The agent should not need this table; it is here for reference.

| State | Meaning | What to do |
|-------|---------|------------|
| `INITIALIZING` | Browser starting up, session loading | Wait a few seconds |
| `QR_READY` | QR scan needed | Call `get_qr` and scan |
| `AUTHENTICATED` | QR scanned, session being established | Wait (→ READY automatically) |
| `READY` | Operational | All tools available |
| `DISCONNECTED` | Connection/session lost | Call `logout` to reset and log in again (no restart) |

### Live socket cross-check

The lifecycle `state` above is driven by whatsapp-web.js **events**, so it can lag behind a session that drops silently. When `state` is `READY`, `status` also queries the live socket (`client.getState()`, a `WAState`) and reports a mismatch with tailored instructions:

| Live `WAState` while READY | Reported as | Fix suggested |
| --- | --- | --- |
| `CONNECTED` | `READY ✅ (ok)` | — |
| `UNPAIRED` / `UNPAIRED_IDLE` | `action needed` | Device unlinked from phone → `logout` + re-scan |
| `CONFLICT` | `action needed` | WhatsApp Web open elsewhere → close it, or `logout` + re-scan |
| `TIMEOUT` | `transient` | May auto-reconnect → wait and re-check; if stuck, `logout` |
| `DEPRECATED_VERSION` | `needs maintenance` | Update the `whatsapp-web.js` dependency (developer task) |
| `getState()` fails / other | `uncertain` | Browser may have crashed → wait and re-check; if stuck, `logout` |

---

## Enable / Disable

### Disable (when not needed)

```
toggle_item(kind="mcp", id="whatsapp", enabled=false)
restart
```

### Re-enable

```
toggle_item(kind="mcp", id="whatsapp", enabled=true)
restart
```

---

## Dependencies

| Package | Version | Purpose |
|---------|---------|---------|
| `whatsapp-web.js` | ^1.34.7 | WhatsApp Web client |
| `puppeteer` | ^25.1.0 | Headless Chromium (bundled) |
| `qrcode` | ^1.5.4 | Generates the QR as PNG / data-URL (HTML) |
| `qrcode-terminal` | ^0.12.0 | ASCII QR fallback |

**System requirements:**
- Node.js ≥ 18
- ~500MB of space for Puppeteer/Chromium
- A background Chromium process while the server is running

---

## Common errors

| Error | Cause | Fix |
|-------|-------|-----|
| `whatsapp-web.js not found` | `npm install` not run | `cd scripts/whatsapp_mcp && npm install` |
| `WhatsApp not ready (status: INITIALIZING)` | Server just started | Wait 15-30 seconds |
| `WhatsApp not ready (status: QR_READY)` | Session expired/missing | Call `get_qr` and scan |
| `WhatsApp not ready (status: DISCONNECTED)` | Connection/session lost | Call `logout`, wait, then `get_qr` and scan |
| Stays `DISCONNECTED` even after restart | Expired cached token in `secrets/whatsapp_session/` | Call `logout` (clears the cache and regenerates the QR) |
| `The browser is already running for …/session` | An orphaned Chromium from a hard-killed (SIGKILL) prior run still holds the profile lock | **Auto-healed on next startup** by `cleanupStaleBrowser()` (kills the orphan + clears the lock). See *Browser lifecycle & restart cleanup* |
| Chat ID not found | Wrong ID | Use `list_chats` to get the correct IDs |

---

## Protocol

Implements JSON-RPC 2.0 over stdio (same pattern as gmail and gcal):
- **Requests:** JSON on stdin (one per line)
- **Responses:** JSON on stdout
- **Logs:** stderr with the `[whatsapp_mcp]` prefix

Supported methods: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`

---

## When to update this file

- New tools added to the server
- Changed session/QR paths under `secrets/`
- New connection states
- Changed dependency versions
