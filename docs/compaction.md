# Context Compaction

## Overview

As a conversation grows, the LLM context window fills up with old messages that
are no longer directly relevant. This increases latency and cost, and eventually
hits the model's token limit.

**Context compaction** solves this by periodically summarising the older portion
of the history into a dense text block. Only the summary and the most recent raw
messages are sent to the LLM on subsequent turns.

The feature is **opt-in** via `config.yml` and is **disabled by default**.

---

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│  handle_message()                                                │
│    1. Check last_input_tokens > threshold                        │
│    2. Call ContextCompactor::try_compact()        ←── new        │
│    3. Run normal LLM loop (build_openai_messages injects summary)│
│    4. Store input_tokens in last_input_tokens     ←── new        │
└──────────────────────────────────────────────────────────────────┘
         │
         ▼
┌──────────────────────────────────────────────────────────────────┐
│  ContextCompactor                          src/compactor.rs      │
│  ────────────────────────────────────────────────────────────── │
│  • Stateless service, shared via Arc across all sessions         │
│  • System prompt hard-coded (not an AGENT.md agent)              │
│  • Uses LlmManager::resolve(strength) for AUTO model selection   │
│  • Persists result to chat_summaries DB table                    │
│  • Publishes BusEvent::CompactionDone to ChatEventBus            │
└──────────────────────────────────────────────────────────────────┘
         │
         ▼
┌──────────────────────────────────────────────────────────────────┐
│  DB: chat_summaries                        src/db/chat_summaries │
│  id │ stack_id │ content │ covers_up_to_message_id │ created_at  │
└──────────────────────────────────────────────────────────────────┘
         │ read by
         ▼
┌──────────────────────────────────────────────────────────────────┐
│  build_openai_messages()              src/session/handler/       │
│                                       messages.rs                │
│  if summary exists:                                              │
│    • inject <conversation_summary> after system prompt           │
│    • load only messages with id > covers_up_to_message_id        │
│  else: load all (current behaviour)                              │
│  if compaction disabled: apply max_history_messages as fallback  │
└──────────────────────────────────────────────────────────────────┘
         │ publishes
         ▼
┌──────────────────────────────────────────────────────────────────┐
│  ChatEventBus: broadcast<BusEvent>     src/chat_event_bus.rs     │
│  BusEvent::UserMessage(ChatEvent)                                │
│  BusEvent::AssistantResponse(ChatEvent)                          │
│  BusEvent::CompactionDone(CompactionEvent)   ←── new             │
└──────────────────────────────────────────────────────────────────┘
         │ consumed by
         ├── HonchoPlugin  (UserMessage, AssistantResponse only)
         └── (future consumers: CompactionDone)
```

---

## Trigger Strategy — Opzione C

Compaction is checked **at the start of each `handle_message` call**, using
the `input_tokens` value from the **previous** turn (stored in
`last_input_tokens: AtomicU32` on `ChatSessionHandler`).

This means:
- Turn N uses many tokens → `last_input_tokens` is stored after turn N.
- Turn N+1 starts → compaction is triggered **before** the LLM runs.
- The user waits for compaction + the new turn, but sees a single response.

No background task, no lock contention, no concurrency hazard.

### Skipped cases

| Condition | Behaviour |
|---|---|
| `compaction` absent from config | Feature disabled entirely |
| `is_ephemeral = true` (cron, tic) | Skipped — sessions are short-lived |
| `last_input_tokens == 0` (first turn or no usage data) | Character estimate used as fallback |
| Fewer messages than `keep_recent` past the summary boundary | Nothing to summarise, skipped |
| LLM returns empty summary | Skipped, warning logged |

### Manual trigger

Compaction can also be triggered **manually** via `ChatSessionHandler::force_compact()` or
`ChatHub::force_compact(source_id)`. The manual path (`force_compact` on
`ContextCompactor`) skips the threshold check entirely and uses a character-based
token estimate, but still respects the ephemeral guard.

A Telegram `/compact` command is available as a user-facing interface; see
[docs/telegram.md](telegram.md).

---

## Compaction Flow

```
try_compact(pool, session_id, stack_id, last_input_tokens, is_ephemeral)
  │
  ├─ guard: is_ephemeral            → Ok(false)
  ├─ resolve effective_tokens:
  │    if last_input_tokens > 0 → use it
  │    else → estimate_tokens_for_stack (sum of chars / 4)
  ├─ guard: effective_tokens < threshold → Ok(false)
  │
  ├─► do_compact(pool, session_id, stack_id, effective_tokens)
  │
  │  (see below)
  │
  └─ Ok(true/false)


force_compact(pool, session_id, stack_id, is_ephemeral)
  │
  ├─ guard: is_ephemeral            → Ok(false)
  ├─ effective_tokens = estimate_tokens_for_stack()   ← no threshold check
  │
  ├─► do_compact(pool, session_id, stack_id, effective_tokens)
  │
  └─ Ok(true/false)


do_compact(pool, session_id, stack_id, effective_tokens)
  │
  ├─ latest_summary = chat_summaries::latest_for_stack()
  ├─ messages = if latest_summary:
  │               for_stack_since(covers_up_to_message_id)
  │             else:
  │               for_stack()
  │
  ├─ guard: messages.len() <= keep_recent → Ok(false)
  │
  ├─ to_summarise = messages[0 .. len - keep_recent]
  ├─ last_covered_id = to_summarise.last().id
  │
  ├─ full_prompt = format_for_summary(
  │     messages      = to_summarise,
  │     prior_summary = latest_summary.content  (if any)
  │   )
  │   → returns a single string (SUMMARIZER_PREAMBLE + transcript + SUMMARY_TEMPLATE)
  │   → if prior summary exists: iterative-update path ("PREVIOUS SUMMARY: …")
  │   → if first compaction: "Create a structured checkpoint summary…"
  │   → transcript uses Hermes-style labels:
  │       [USER]: …
  │       [ASSISTANT]: … [Tool calls: name(args)]
  │       [TOOL RESULT tc_N]: …
  │     with head+tail truncation (6 000+1 500 for messages, 4 000+1 500 for results)
  │
  ├─ (client_name, llm) = LlmManager::resolve(None, None, config.strength)
  ├─ call llm.chat_with_tools([{role: "user", content: full_prompt}], tools=[], options)
  │     options.temperature = 0.3  (faithful, low-creativity)
  │   (Hermes-style: everything in a single user message, no separate system message)
  │
  ├─ summary_id = chat_summaries::save(stack_id, summary_text, last_covered_id)
  ├─ event_bus.compaction_done(CompactionEvent { ... })
  └─ Ok(true)
```

---

## Database Schema

```sql
CREATE TABLE chat_summaries (
    id                      INTEGER PRIMARY KEY AUTOINCREMENT,
    stack_id                INTEGER NOT NULL REFERENCES chat_sessions_stack(id),
    content                 TEXT    NOT NULL,
    -- All chat_history rows with id <= this value are covered.
    -- build_openai_messages loads only rows with id > this value.
    covers_up_to_message_id INTEGER NOT NULL,
    created_at              TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_chat_summaries_stack ON chat_summaries (stack_id);
```

**One active summary per stack** — multiple rows can exist (each compaction
creates a new one), and the most recent is always used.  There are no nested
summaries: when a second compaction runs, the prior summary body is fed to the
LLM under `PREVIOUS SUMMARY:` in the iterative-update path, and the new row
supersedes it.

---

## LLM Selection

The compactor uses `LlmManager::resolve(None, None, config.strength)` — the
same AUTO selection used for agents.  `strength` maps to the existing tier
system:

| Strength | Typical use |
|---|---|
| `very_low` / `low` | Fastest, cheapest local model |
| `average` | Recommended for summaries |
| `high` / `very_high` | Overkill for summarisation |

The compaction LLM is **not** a registered agent — its prompt constants are
hard-coded in `src/core/compactor.rs` and are not configurable from `agents/` or
AGENT.md files:

| Constant | Role |
| --- | --- |
| `pub const SUMMARY_PREFIX` | Handoff header prepended to the summary when injected as context. Tells the LLM "reference only — resume from `## Active Task`". Also used in `messages.rs`. |
| `SUMMARIZER_PREAMBLE` | Opening of the compaction prompt. Plain wording to avoid content-filter false positives on Azure/OpenAI-compatible providers. |
| `SUMMARY_TEMPLATE` | 13-section structured template the LLM must fill. Ported from [Hermes agent](https://github.com/NousResearch/hermes-agent). |

---

## ChatEventBus Extension

The `ChatEventBus` was extended from `broadcast<ChatEvent>` to
`broadcast<BusEvent>`, a new enum:

```rust
pub enum BusEvent {
    UserMessage(ChatEvent),
    AssistantResponse(ChatEvent),
    CompactionDone(CompactionEvent),   // new
}
```

Existing consumers (`honcho` plugin) were updated to match on
`BusEvent::UserMessage` / `BusEvent::AssistantResponse` and ignore
`CompactionDone`.  Future consumers can subscribe and react to compaction
events (e.g. to flush external memory, reset embeddings, etc.).

---

## Configuration

```yaml
# config.yml  (under the llm: section)
llm:
  # ... existing settings ...

  compaction:
    # Trigger compaction when the previous turn used more than this many
    # input tokens. Required.
    threshold_tokens: 30000

    # Number of recent raw messages to keep outside the summary. Default: 6.
    keep_recent: 6

    # Minimum LLM strength for summary generation (AUTO selection).
    # Summaries are simple writing tasks — low or average is sufficient.
    # Omit to use whatever AUTO picks.
    strength: low
```

### Recommended values by use case

| Scenario | `threshold_tokens` | Notes |
|---|---|---|
| Local 8k model (LM Studio) | 5 000 – 6 000 | Compact early and often |
| Local 32k model | 20 000 – 25 000 | Leave room for the summary itself |
| Claude Sonnet (200k) | 100 000 – 150 000 | Or omit compaction entirely |
| Claude Haiku (200k) | 80 000 | Cheaper per call; compact less often |

### Provider without token usage (e.g. some LM Studio setups)

If the LLM provider does not return `input_tokens` in the response, the
compactor falls back to estimating token usage as `total_chars / 4`.  Set a
lower `threshold_tokens` when relying on this estimate since it underestimates
non-ASCII content.

---

## Known Limitations

- **Sub-agent stacks**: compaction applies only to the root session stack
  (`depth = 0`). Sub-agent stacks are typically short-lived and do not benefit
  from compaction.
- **Tool results in summary**: the serialiser uses a head+tail strategy —
  message bodies are truncated to 6 000+1 500 chars, tool results to
  4 000+1 500 chars, tool-call arguments to 1 200 chars. Very long outputs
  are preserved at both ends; only the middle is replaced with `...[truncated]...`.
- **Tool result pruning in live context**: `maybe_hide_tool_result` replaces
  oversized previous-turn results with an informative 1-line summary
  (e.g. `[execute_cmd] ran \`cargo build\` → exit 0, 47 lines output`) rather
  than a generic "hidden" placeholder. No LLM call — pure string parsing.
- **Frontend visibility**: the compaction step is transparent to the user.
  No `Compacting` event is sent to the WebSocket. The `BusEvent::CompactionDone`
  event is available on the internal bus for future subscribers.
- **Cold restart**: `last_input_tokens` is stored in memory (`AtomicU32`), not
  in the DB. After a restart, the first turn of a session will not trigger
  compaction even if the history is already long. The second turn will trigger
  it correctly once the LLM reports usage.
