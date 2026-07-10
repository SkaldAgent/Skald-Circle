# Notifications

## Overview

TIC is a background agent that runs every 15 minutes, reads incoming events (email, WhatsApp, Google Calendar) and decides what is worth surfacing to the user. Its filtering behaviour is controlled by the file `data/notifications.md`.

When TIC runs, that file is **automatically injected into its system prompt** (via `inject_memory` in `agents/tic/meta.json`). TIC reads the rules before evaluating each event batch.

---

## How notifications reach the main agent

TIC (and other background agents like cron workers) calls the `notify` interface tool with a **structured notification** — `{source, event_type, summary, event_time, refs}` (the `Notification` struct in `src/core/notification.rs`) — queued via `ChatHub::notify()`. The `summary` is a neutral, third-person statement of fact; the **main agent** owns the user-facing wording. The notification delivery chain:

1. `ChatHub::notification_consumer` batches notifications (200 ms window), then:
2. Appends a **synthetic Assistant message** to `chat_history` (`is_synthetic=true`) with a reasoning trace in `reasoning_content`:
   > "The system signaled pending notifications. Let me read them and surface anything relevant to the user."
3. Inserts a **pre-completed tool call** in `chat_llm_tools`: `read_notification()` with `status='done'`, `result_type='json'`, and `result` containing the JSON array of notification objects.
4. Calls `ChatHub::resume()` → `resume_turn` detects the synthetic tool call on the last assistant message and runs the LLM loop.
5. The conversational agent (depth=0) sees the tool result as if it had just called `read_notification`, then writes the user-facing message — naming the source and adding context, rather than echoing the raw `summary`.

The `read_notification` tool is a real `Tool` registered in `ToolRegistry` (category `Introspection`, `root_agent_only=true`). When the LLM calls it normally, it returns an empty array `[]` — notifications are only ever injected synthetically by `ChatHub`. The tool is visible only to depth=0 agents (filtered out of sub-agent configs via `root_only_tool_names`).

This replaces the previous mechanism (synthetic User message with `[SYSTEM - NOTIFICATION]` prefix and `extra_system_dynamic` framing instructions). The new approach:
- Uses the **natural tool-call pattern** (tool → result → response)
- Applies to the **assistant itself** (synthetic reasoning + tool call)
- Avoids injecting fake user turns and behavioural framing in system messages

---

## How the main agent updates preferences

When the user says something like:
- *"Notify me when I get an email from Mario"*
- *"Stop alerting me about group WhatsApp messages"*
- *"Always tell me if a meeting starts in less than an hour"*

The main agent must **update `data/notifications.md`** directly using `edit_file` or `write_file`.

No restart is needed — TIC reads the file fresh on every tick.

---

## File format

The file is plain Markdown. TIC reads it as free prose, so any clear natural-language instruction works. The recommended structure is:

```markdown
# Notification preferences

## Always notify
- <rule>
- <rule>

## Never notify
- <rule>
- <rule>

## Custom rules
- <rule>
- <rule>
```

### Section semantics

| Section | Meaning |
|---|---|
| `## Always notify` | Events matching these rules should always be surfaced, even if they seem low-priority by default |
| `## Never notify` | Events matching these rules should be silently dropped, even if they seem important by default |
| `## Custom rules` | Everything else: conditional rules, time-based rules, contact-specific overrides |

---

## Examples

```markdown
## Always notify
- Emails from Kandice Phillips or anyone at Dawson Cornwell
- Any calendar event that was added today and starts within 24 hours
- WhatsApp messages from my sister (saved as "Valentina")

## Never notify
- Newsletters, marketing, automated digests
- LinkedIn notifications
- Calendar reminders for recurring weekly meetings

## Custom rules
- Only notify about WhatsApp group messages if I am explicitly mentioned
- For emails about the Serena legal matter, always notify regardless of sender
```

---

## How TIC uses this file

TIC reads the rules at the **Step 1 — Read memory** phase (the file is pre-injected in context). It then applies them during **Step 3 — Decide**:

- A rule in `Always notify` raises the threshold for suppression — TIC will surface the event unless something is clearly spam.
- A rule in `Never notify` acts as a hard filter — TIC drops the event without calling `notify`.
- `Custom rules` are evaluated as natural-language conditions. TIC is instructed to interpret them strictly and not over-generalise.

If the file does not exist or is empty, TIC falls back to its built-in heuristics (see `agents/tic/AGENT.md`).

---

## File location

```
data/notifications.md
```

This file lives in `data/` (not `data/memory/`) because it is system configuration, not personal memory. It is user-editable and version-control friendly.

---

## When to Update This File

- The notification delivery mechanism changes (e.g. `read_notification` tool, synthetic injection flow)
- TIC's behaviour or decision process is modified
- A new background agent that calls `notify` is introduced
- The `data/notifications.md` format or location changes
