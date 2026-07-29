# Honcho Memory

- **Plugin id:** `honcho`
- **Category:** Long-term memory (external service)
- **Runs:** talks to a Honcho server (self-hosted or hosted) over HTTP — needs network access to it

## What it does

Streams a user's completed chat turns to an external [Honcho](https://honcho.dev) server so it can build long-term memory about them across sessions — facts, preferences, a curated "peer card" — and feeds retrieved context back into future turns automatically. Also adds tools the assistant can call directly: `memory_query`, `honcho_profile`, `honcho_search`, `honcho_context`, `honcho_conclude`.

**This is different from the app's built-in private memory** (`user-memory/…`, stored encrypted in the user's own database). Honcho is an external system and stores conversation content **in cleartext**, so it is **strictly opt-in per user and off by default** — enabling the plugin does nothing on its own until each individual user turns it on for themselves.

## Requirements

- A running Honcho server reachable from this machine (local install or hosted), and its base URL.
- Optionally an API key, if that Honcho instance requires one.

## Enabling & configuring (admin)

1. Plugins page → **Honcho Memory** → enable, then **Configure** (or its own admin page, once enabled: sidebar → Honcho).
2. Fields:
   - **`base_url`** (default `http://localhost:8000`) — the Honcho server's URL.
   - **`api_key`** — optional, only if the server requires auth.
   - **`workspace_id`** (default `skald-circle`) — a name identifying this instance inside Honcho. Each user becomes a separate "peer" inside the same workspace. Use a fresh, unique name for a new instance.
3. The admin config page includes a connectivity test.

## Per-user setup

Long-term memory is **off for every user until they turn it on themselves**. Once the plugin is enabled and the user has been granted access (admin: Users → that person → **Plugins** → tick Honcho), they'll see a **"Long-term memory"** page in their sidebar with a single opt-in toggle. If a user asks the assistant to "remember things long-term" or asks why it doesn't remember past conversations, and this plugin is enabled, point them to that page rather than trying to enable it on their behalf.

## Notes

- Explain the privacy trade-off honestly if a user asks: their messages get stored in cleartext on the Honcho server, outside the encrypted database this app otherwise uses. Some users may not want that.
- Both the "remembering" (write) and "recalling" (read/search tools) sides of this plugin are gated on the same opt-in flag — a user who hasn't opted in gets a clear "not opted in" response instead of a silent no-op.
