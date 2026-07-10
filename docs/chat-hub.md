# ChatHub

`ChatHub` (`src/core/chat_hub/`) is the single entry point for **interactive, user-facing chat sessions** — web, mobile, Telegram, project chats. It owns the `source → session` mapping, serializes incoming user messages per source (injecting them into an in-flight turn where possible), runs each turn through a `ChatSessionHandler`, and bridges every turn's events onto the global broadcast bus that connected clients subscribe to.

## What it is — and is not

ChatHub manages **one live, persistent session per `source`**, addressed by source id through the `sources` table. It is **not** a runner for background / non-interactive agents:

- Cron jobs, TIC ticks, and async sub-agent tasks go through `TaskManager` / `ChatSessionManager` directly. They are not user-facing, have no broadcast audience, and must not appear in the `sources` table.
- The one bridge from background → interactive is **notifications**: a background agent calls `ChatHub::notify(...)`, and the notification consumer delivers aggregated structured notifications to the *home* source (see below).

Keep that boundary: routing a non-interactive agent through ChatHub is a misuse.

## Source → session mapping

The `sources` table (`src/core/db/sources.rs`) maps each `source_id` to its `active_session_id`. `get_or_create_session` looks it up and lazily creates a session on first use; `provision_session(reset=true)` discards the current session and starts a fresh one (emitting a `NewSession` event). `clear(source)` is the thin wrapper used by `/clear` / "new conversation".

## Per-source inbox (serialization + live injection)

Each source gets one **`SourceInbox`** and one **consumer task**, created lazily on the first message (`src/core/chat_hub/inbox.rs`). This sits *in front of* the handler's `processing` mutex and gives two properties the old per-message detached-spawn dispatch lacked:

1. **FIFO ordering** — a single consumer per source means arrival order = execution order. (Previously each message was a detached `tokio::spawn` racing for the `processing` lock, so order was not guaranteed.)
2. **Live injection** — messages that arrive while a turn is running are **injected into the running turn** at its next round boundary (see [mid-turn injection](#mid-turn-injection-live-steering)), rather than waiting for a separate follow-up turn. Messages are kept as **individual** rows; coalescing for the LLM happens later in the `MessageBuilder` (merging consecutive user rows into one `role:user`), not in the inbox.

### Flow

```text
send_message(source, prompt, opts)
  → push QueuedMessage onto the source's inbox, notify, return immediately
        (turn errors surface via the Error event on the bus, not the return value)

[per-source consumer task]
  → wait for notify (+ optional debounce window)
  → loop: build_unit(pending) → dispatch_turn(...) until the queue is empty
        build_unit pops ONE message to seed a turn (no coalescing)
        dispatch_turn = resolve session/handler, bridge events to the global bus,
        inject execute_task, build the PendingUserInput handle (real user turns
        only), call handler.handle_message (takes the processing lock)
```

The consumer holds the inbox lock **only** while building a unit (never during the turn). While a turn runs, the turn itself drains `pending` at each round boundary via the `PendingUserInput` handle, so new messages are injected live; only messages that arrive *after* the turn's last boundary remain in `pending` and seed the next turn on a following iteration. The consumer and the in-flight turn never touch `pending` concurrently — the consumer is parked awaiting `dispatch_turn`.

**Panic isolation** — each turn runs on its own `tokio::spawn`ed task whose `JoinHandle` the consumer awaits. A panic inside a turn therefore surfaces as a `JoinError` (logged as *"source turn panicked — consumer surviving"*) instead of unwinding the consumer task itself. Without this, a single panicking turn would silently kill the source's consumer: subsequent messages would enqueue and `notify()`, but nothing would ever drain them — the chat would appear frozen with no error. (Panics are also routed to the log file via a `std::panic::set_hook` in `main.rs`; the default hook only writes to stderr, i.e. the `run.sh` terminal.)

### Inbox helpers (`inbox.rs`)

- `build_unit(pending)` — pops a **single** message (no coalescing) to seed a turn. Empty queue → `None`.
- `drain_leading_user(pending)` — drains the leading run of consecutive **non-synthetic** messages, returning them individually; stops at the first **synthetic** message (`opts.is_synthetic` — notifications / TIC), which is left for the notification path. Used by the running turn (via `InboxUserInput: PendingUserInput`) to inject queued user input at a round boundary.

### Idle debounce

`SOURCE_COALESCE_DEBOUNCE_MS` (in `mod.rs`) defaults to **0**: a message to an idle source dispatches immediately. Raising it batches messages sent rapidly to an *idle* source, at the cost of that latency on the first message of a burst.

### `/stop` and `/clear`

- **`cancel(source)`** (`/stop`, stop button) clears the inbox's pending queue *and* cancels the in-flight turn (`handler.cancel()`). A `cancel_epoch` counter guards the tiny window where the consumer drained a unit microseconds before the stop — the stale unit is dropped instead of dispatched.
- **`clear(source)` / `provision_session(reset=true)`** (`/clear`, new conversation) also drops messages queued for the discarded session.

## Event bus

ChatHub owns a single global broadcast channel (`global_tx`, capacity 512). Every turn gets a fresh mpsc sender via `bridge_to_global`, which forwards the handler's `ServerEvent`s onto the global bus wrapped in a `GlobalEvent { source, session_id, event }`. Subscribers (`events(source)`) filter by source themselves — e.g. the WebSocket handler and the Telegram `persistent_forwarder`. `emit(...)` posts a sessionless event directly.

## Notifications (background → home source)

`notify(Notification)` / `notify_sync(Notification)` push a structured `Notification` (`src/core/notification.rs`: `{source, event_type, summary, event_time, refs}`) onto a central mpsc queue. The `notification_consumer` task batches bursts over `NOTIFY_BATCH_WINDOW_MS` (200 ms), then delivers them to the *home* source (`set_home` / `HOME_SOURCE_KEY`, default `web`) by appending a synthetic Assistant message with a pre-completed `read_notification` tool call (result = JSON array of `Notification` objects, `result_type='json'`) and calling `resume(...)`. This path uses `resume`, not `send_message`, so it does not go through the per-source inbox.

## API surface (`ChatHubApi` trait)

Defined in `crates/core-api/src/chat_hub.rs`, implemented on `ChatHub`:

| Method | Purpose |
|--------|---------|
| `send_message` | Enqueue a user message for a source (async; injected into an in-flight turn, or seeds a new one) |
| `register` | Register a source (no-op with the global bus) |
| `clear` | New session for the source, discard the previous one |
| `cancel` | Stop the in-flight turn + clear the queued backlog |
| `resume` | Resume an interrupted turn (pending tools / async result injection) |
| `reset_mcp` | Revoke all session-scoped tool-group grants (MCP servers + `config`); exposed to users as `/resettools` |
| `set_home` | Set which source receives background notifications |
| `context_info` / `cost_info` | Last-turn token usage / total session spend |
| `force_compact` | Force context compaction now |
| `events` | Subscribe to the global event bus |
| `resolve_question` | Answer a pending `ask_user_clarification` |
| `approve` / `reject` | Resolve a pending tool-call approval |

## Mid-turn injection (live steering)

A message sent while a turn is already running is delivered *into* that turn instead of waiting for it to finish. This works because the LLM loop rebuilds history fresh from the DB each round (`llm_loop.rs`), so a `user` row appended at a round boundary is picked up by the next round automatically.

- `dispatch_turn` builds an `InboxUserInput` (an `Arc<dyn PendingUserInput>` wrapping the `SourceInbox`) and passes it into `handle_message` → `run_agent_turn`. It is `Some` only for real user turns (never synthetic), and only the **root** turn receives it — sub-agents, resume, and non-interactive runners pass `None`.
- At the top of each round, `run_agent_turn` calls `drain_user()` and appends each queued message as its own `chat_history` `user` row, then emits a `UserMessage` event carrying the new `message_id` (telnet-style echo — see [frontend.md](frontend.md)). A round boundary is the only clean ordering point: the previous round's assistant message and its tool results are all persisted, so a `user` row appended there is well-ordered (never between an assistant tool-call and its tool results).
- Injection does **not** interrupt the in-flight LLM call or tool, and does **not** reset the round budget (`max_tool_rounds`).
- `MessageBuilder` merges consecutive non-failed `user`/`agent` rows into one `role:user` for the LLM, so several injected messages read as a single clean user turn while the DB keeps them distinct.
- `/stop` clears `pending` (`clear_inbox`): queued-but-not-yet-injected messages are dropped, never persisted, never echoed.

## Relevant files

| Path | Role |
|------|------|
| `src/core/chat_hub/mod.rs` | `ChatHub`: API, dispatch, event bridge, notification consumer, per-source consumer |
| `src/core/chat_hub/inbox.rs` | `SourceInbox`, `QueuedMessage`, `build_unit` (single pop) + `drain_leading_user` (mid-turn injection) + tests |
| `src/core/db/sources.rs` | `source → active_session_id` mapping |
| `crates/core-api/src/chat_hub.rs` | `ChatHubApi` trait + `SendMessageOptions` |
| `src/core/session/handler/` | `ChatSessionHandler` — the turn itself (see [session.md](session.md)) |

## When to update this file

- New `ChatHubApi` methods or changed dispatch flow
- Changes to inbox draining (`build_unit` / `drain_leading_user`) or the debounce constant
- Changes to `/stop` / `/clear` queue semantics
- Changes to mid-turn injection (round-boundary drain, `PendingUserInput`)
