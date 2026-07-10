# Honcho Memory Plugin

Plugin: `crates/plugin-honcho/src/lib.rs`
HTTP client: `crates/honcho-client/` (separate workspace crate)

---

## Purpose

Streams completed chat turns to a [Honcho](https://honcho.dev) server so that it can extract long-term conclusions about the user (write path), and reads that context back into every LLM turn via the [`Memory`](memory.md) trait (read path).

---

## Self-hosted Docker package

A ready-to-run Docker Compose setup is in the [`honcho/`](../honcho/) folder at the project root.
It starts four services: the Honcho API, the deriver background worker, PostgreSQL + pgvector, and Redis.

**Quick start:**

```sh
cd honcho
cp .env.example .env
# Edit .env — set at least LLM_OPENAI_API_KEY=sk-...
docker compose up -d
# API available at http://localhost:8000
```

Full instructions, LLM provider options (OpenAI, OpenRouter, Ollama), and troubleshooting are in [`honcho/README.md`](../honcho/README.md).

---

## Setup

1. Start the Honcho server (see above).
2. Enable the plugin via the agent or REST API:
   ```json
   PUT /api/plugins/honcho
   {
     "enabled": true,
     "config": {
       "base_url":     "http://localhost:8000",
       "api_key":      "",
       "workspace_id": "personal-agent"
     }
   }
   ```
   Or ask the main agent: _"enable the honcho plugin"_.

---

## Configuration

Stored in the `plugins` SQLite table (`config` JSON blob). Managed at runtime — no entry in `config.yml`.

| Field | Type | Default | Description |
| --- | --- | --- | --- |
| `base_url` | string | `http://localhost:8000` | Honcho server URL |
| `api_key` | string | _(empty)_ | API key; leave empty for local/unauthenticated instances |
| `workspace_id` | string | `personal-agent` | Honcho workspace identifier for this agent instance |

---

## Honcho Object Model

```
workspace  (workspace_id from config — one per agent instance)
├── peer  "user"       observe_others=true
├── peer  "assistant"  observe_me=true
└── session            one per local chat_sessions.id
    ├── message  peer_id="user"
    ├── message  peer_id="assistant"
    └── …
```

**Workspace and peers** are created (idempotently) each time the plugin starts. If they already exist, the API returns an error which is logged at `WARN`/`DEBUG` and ignored.

**Sessions** are created lazily on the first event for a new `chat_sessions.id`, then cached in memory for the life of the listener task. The Honcho session UUID is stored in the session cache but not persisted to SQLite — restarting the plugin creates new Honcho sessions for subsequent events.

---

## Event Filtering

An event is forwarded only when **all** of the following conditions hold:

| Condition | Reason |
| --- | --- |
| `is_interactive = true` | A real user is in the conversation |
| `is_ephemeral = false` | Not a short-lived automated session (cron, tic) |
| `is_synthetic = false` | Message content was typed by the user, not injected by the system |
| `role` is `User` or `Assistant` | Sub-agent messages (`Agent` role) are skipped |
| `content` is non-empty | Guard against empty strings |

---

## Lifecycle

1. **`start()`** — subscribes to `skald.event_bus`, calls `ensure_workspace_ready` (best-effort), then spawns the listener task.
2. **Listener task** — `tokio::select!` loop on the bus receiver and a `CancellationToken`. On `RecvError::Lagged`, logs a warning and continues (some turns are missed but the task stays alive).
3. **`stop()`** — cancels the token and awaits the task.
4. **`reload()`** — follows the standard plugin pattern: start/stop/restart-on-change.

---

## Error Handling

All Honcho API errors are **fire-and-forget**: logged as `warn!` and never propagated to the session handler or the user. A Honcho outage has zero impact on chat functionality.

`HonchoError::Request`'s `Display` walks the full `source()` chain, so transport
failures surface the real cause in logs (e.g. `Request failed: error sending
request for url (...): Connection reset by peer`) instead of just reqwest's
opaque top-line. This makes host↔container issues (e.g. a stale Docker Desktop
port-forward after a container recreation) diagnosable from the `warn!` alone.

---

## Read Path

`HonchoMemory` implements the [`Memory`](../memory.md) trait. Before each LLM turn,
`query_context` is called automatically by `ChatSessionHandler::handle_message` — for
**all** session types: interactive, cron, and tic.

### Flow

1. Checks `is_available()` — returns `None` immediately if the plugin is stopped.
2. Looks up the Honcho session UUID for the local `session_id` in the shared `session_map`.
3. **If a mapping exists** (interactive session with at least one turn written):
   - Calls `client.session_context(workspace_id, honcho_session_id, tokens=2000, search_query=user_msg)`.
   - Returns the formatted result on success.
   - On error: logs `warn!` and falls through to the peer-context fallback **without** `search_query`
     (avoids a second embedding of the same user message — `session_context` already embedded it before failing).
4. **Fallback — `peer_context("user")`** (no mapping, or session_context error):
   - Cold start / cron / tic (no `session_map` entry): calls with `search_query=user_msg` for relevance.
   - After a `session_context` failure: calls **without** `search_query` to avoid double-embedding.
   - Returns global user knowledge derived from all sessions Honcho has observed.
   - On error: logs `warn!` and returns `None`.

The formatted context is prepended to `extra_system_context` and injected into the system prompt. Errors are never propagated — they degrade gracefully to `None`.

### Context format

`format_context()` extracts, in priority order:

1. `conclusions[].content` → "Known facts about the user: …"
2. `summary` → "Conversation summary: …"
3. Fallback: pretty-printed raw JSON

The result is wrapped in `--- Honcho memory context --- / --- end ---` markers.

---

## LLM-callable Tools

`HonchoMemory::tools()` returns **five** tools whenever the plugin is active
(`is_available()` true). They give the LLM direct, on-demand access to every
layer of Honcho's API, complementing the automatic pre-turn `query_context`
injection. All operate on the `user` peer and are inherited by sub-agents via
`AgentRunConfig::memory_tools`.

The [official Honcho documentation](https://honcho.dev/docs/v3/documentation/features/chat) recommends exposing these as tools so the agent decides on its own when to read or write memory, rather than only relying on automatic injection.

| Tool | Endpoint | Cost | What it does |
| --- | --- | --- | --- |
| `memory_query` | `POST .../peers/user/chat` | High (LLM synthesis) | Natural-language question → synthesized answer (dialectic reasoning, `reasoning_level=low`) |
| `honcho_search` | `GET .../peers/user/context?search_query=…` | Low | Semantic search over derived facts; returns raw ranked excerpts (with ids when present) |
| `honcho_context` | `GET .../peers/user/context` | Low | Full context snapshot (conclusions + summary), no synthesis; optional focus `query` |
| `honcho_profile` | `GET`/`PUT .../peers/user/card` | Low | Read the peer card, or overwrite it with a list of fact strings (`card`) |
| `honcho_conclude` | `POST .../conclusions` / `DELETE .../conclusions/{id}` | Low | Write a new fact (`conclusion`) or delete one by id (`delete_id`); exactly one required |

**Peer model — all tools operate on the `user` peer as both observer and observed.**
This plugin configures the `user` peer with `observe_me = true`, so the user's
self-knowledge lives in the `observer = user / observed = user` slot. Therefore
`honcho_conclude` writes with `observer_id = observed_id = user`, and `honcho_search`
uses `peer_context` (not the `conclusions/query` endpoint, which requires explicit
observer/observed filters) — the same proven path as the automatic read-path
injection. This differs from setups where the assistant observes the user
(`observer = assistant`); keeping observer = user is what lets the read-path see
facts written by `honcho_conclude`.

**When to use vs. the automatic injection:**

| Mechanism | When it fires | Best for |
| --- | --- | --- |
| `query_context` (auto) | Before every LLM turn | Background context, cold-start facts |
| `memory_query` (tool) | LLM calls it explicitly | On-demand deep reasoning mid-conversation |
| `honcho_search` / `honcho_context` (tools) | LLM calls them explicitly | Cheap raw recall without LLM synthesis |
| `honcho_profile` / `honcho_conclude` (tools) | LLM calls them explicitly | Actively curating long-term memory |

**Implementation note:** `Tool::execute` is synchronous but the Honcho calls are
async. All five tools share the `run_blocking` helper, which uses
`tokio::task::block_in_place` + `Handle::current().block_on(...)` to drive the
future from within the Tokio multi-thread scheduler without spawning a new thread.

---

## Future Work

- **Session persistence** — store the Honcho session UUID in a new `chat_sessions.honcho_session_id` column so the mapping survives a plugin restart.

---

## When to Update This File

- Config fields change
- Honcho object model or peer setup changes
- Filtering rules change
- `query_context` flow changes (session vs peer fallback logic)
- Docker Compose setup in `honcho/` changes significantly
- Public API of `crates/honcho-client/` changes
