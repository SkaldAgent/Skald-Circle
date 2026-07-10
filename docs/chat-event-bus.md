# Event Buses

Two independent in-process buses:

| Bus | Type | Purpose |
| --- | ---- | ------- |
| **Chat** | `ChatEventBus` | Chat-turn events (user messages, assistant responses, compaction) |
| **System** | `SystemEventBus` | Infrastructure lifecycle events (provider registration, etc.) |

See below for details on each.

---

## Chat Event Bus

`src/core/chat_event_bus.rs` — thin re-export of `crates/core-api/src/bus.rs`.
All types (`ChatEventBus`, `BusEvent`, `ChatEvent`, `CompactionEvent`, etc.) are defined in `core-api` and re-exported here for backward compatibility.

## Purpose

Decouples producers (session handlers, compactor) from consumers (Honcho
memory, future analytics, etc.). Events are published **only after an operation
completes successfully** — messages are already persisted in the DB when the
event fires.

---

## Top-level Type: `BusEvent`

```rust
pub enum BusEvent {
    UserMessage(ChatEvent),
    AssistantResponse(ChatEvent),
    CompactionDone(CompactionEvent),
}
```

Consumers match on the variant to decide what to handle:

```rust
match event {
    BusEvent::UserMessage(e) | BusEvent::AssistantResponse(e) => { /* ... */ }
    BusEvent::CompactionDone(e) => { /* optional */ }
}
```

---

## Sub-types

### `ChatEvent`

Published once per completed turn — one `UserMessage` and one
`AssistantResponse` in order.

| Field | Type | Description |
| --- | --- | --- |
| `session_id` | `i64` | `chat_sessions.id` |
| `stack_id` | `i64` | `chat_sessions_stack.id` |
| `message_id` | `i64` | `chat_history.id` for this message |
| `role` | `ChatEventRole` | `User`, `Assistant`, or `Agent` |
| `content` | `String` | Message text |
| `is_synthetic` | `bool` | `true` when the *message* is system-generated (TicManager ticks, ChatHub briefings) |
| `is_interactive` | `bool` | `true` when a real user participates (web, Telegram) |
| `is_ephemeral` | `bool` | `true` for short-lived automated sessions (cron, tic) |
| `tool_calls` | `Vec<ToolCallEvent>` | Non-empty only for `Assistant` events |
| `created_at` | `DateTime<Utc>` | Timestamp of publication |

### `ToolCallEvent`

| Field | Type | Description |
| --- | --- | --- |
| `name` | `String` | Tool name |
| `arguments` | `Option<String>` | JSON-serialized arguments |
| `result` | `Option<String>` | Tool output or error message |
| `status` | `String` | `"done"` or `"failed"` |

### `CompactionEvent`

Published by `ContextCompactor` after a summary is persisted to `chat_summaries`.

| Field | Type | Description |
| --- | --- | --- |
| `session_id` | `i64` | `chat_sessions.id` |
| `stack_id` | `i64` | `chat_sessions_stack.id` |
| `summary_id` | `i64` | `chat_summaries.id` of the new row |
| `covers_up_to_message_id` | `i64` | Boundary: messages with `id > this` are loaded raw from now on |
| `triggered_by_tokens` | `u32` | Input token count that triggered this compaction |

---

## ChatEventBus

```rust
// Instantiated once in main.rs, stored in `Skald` and ChatSessionManager.
let bus = Arc::new(ChatEventBus::new()); // default capacity: 256

// Publish (called internally):
bus.user_message(event);           // from ChatSessionHandler
bus.assistant_response(event);     // from ChatSessionHandler
bus.compaction_done(event);        // from ContextCompactor

// Subscribe (call once at startup, loop in tokio::spawn):
let mut rx = bus.subscribe();
tokio::spawn(async move {
    loop {
        match rx.recv().await {
            Ok(BusEvent::UserMessage(e))      => { /* process */ }
            Ok(BusEvent::AssistantResponse(e)) => { /* process */ }
            Ok(BusEvent::CompactionDone(e))    => { /* optional */ }
            Err(RecvError::Lagged(n))          => warn!("lagged by {n} events"),
            Err(RecvError::Closed)             => break,
        }
    }
});
```

---

## Publication Rules

### Chat events

| Source | `is_synthetic` | Published? |
| --- | --- | --- |
| User via WebSocket / Telegram | `false` | ✅ on `TurnOutcome::Final` |
| Cron job | `false` | ✅ on `TurnOutcome::Final` |
| TicManager tick | `true` | ✅ on `TurnOutcome::Final` |
| ChatHub notification briefing | `true` (injected via `resume_turn`) | ❌ never (uses `resume_turn`, not `handle_message`) |
| `TurnOutcome::Cancelled` | — | ❌ never |
| `TurnOutcome::Exhausted` | — | ❌ never |
| Sub-agent turns (`dispatch_sub_agent`) | — | ❌ never (only root turns publish) |

Per each successful turn, **two events** are published in order:

1. `BusEvent::UserMessage` — content of the user message
2. `BusEvent::AssistantResponse` — content of the assistant response + tool calls

### Compaction events

`BusEvent::CompactionDone` is published by `ContextCompactor::try_compact()`
whenever a new summary is successfully persisted. It fires **before** the LLM
call that processes the user's message (Opzione C trigger). See
[compaction.md](compaction.md) for the full flow.

---

## Adding a Consumer

1. Call `skald.event_bus.subscribe()` in `main.rs` after `Skald::new()` completes.
2. Spawn a background task with a receive loop.
3. Match on `BusEvent` variants — ignore variants you don't care about.
4. On `RecvError::Lagged`, log and continue — do not panic.
5. Keep the consumer fast; if it does I/O (HTTP calls), buffer or batch internally.

---

## Channel Capacity

Default: **256 events** (`DEFAULT_CAPACITY` in `chat_event_bus.rs`).  If a
consumer falls behind by more than 256 events it receives `Lagged` errors and
misses intermediate events.  Tune via `ChatEventBus::with_capacity(n)`.

---

## When to Update This File

- New variants added to `BusEvent`
- New fields added to `ChatEvent`, `ToolCallEvent`, or `CompactionEvent`
- Publication rules change (new sources, new conditions)
- A new consumer is wired up in `main.rs`

---

## System Event Bus

`crates/core-api/src/system_bus.rs` — infrastructure lifecycle events, separate from chat-turn events.

### `SystemEvent`

```rust
pub enum SystemEvent {
    ApiProviderRegistered   { type_id: String },
    ApiProviderUnregistered { type_id: String },
}
```

### Wiring

- **Created** early in `main.rs` (no dependencies), stored in `skald.system_bus` and `PluginContext::system_bus`.
- **Producers**: `ProviderRegistry::register_plugin()` / `unregister_plugin()` emit automatically — plugins do not need to touch the bus directly.
- **Consumers**: `TtsManager` and `TranscribeManager` subscribe at construction time and call `reload()` on `ApiProviderRegistered` / `ApiProviderUnregistered`. This ensures DB-backed models whose provider was not yet in the registry at startup are picked up as soon as the plugin starts.

### Adding a consumer

```rust
let mut rx = system_bus.subscribe();
tokio::spawn(async move {
    loop {
        match rx.recv().await {
            Ok(SystemEvent::ApiProviderRegistered { type_id })   => { /* ... */ }
            Ok(SystemEvent::ApiProviderUnregistered { type_id }) => { /* ... */ }
            Err(RecvError::Lagged(n)) => warn!("system_bus lagged by {n}"),
            Err(RecvError::Closed)    => break,
        }
    }
});
```

### Adding a new `SystemEvent` variant

1. Add the variant to `SystemEvent` in `crates/core-api/src/system_bus.rs`.
2. Emit it from the relevant producer.
3. Update consumers that need to react.
4. Update this file.
