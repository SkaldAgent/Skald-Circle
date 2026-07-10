# Session & Message Handling

**RunContext** (approval policy, system prompt injection, file-write pre-authorization, working directory) is documented separately — see [run-context.md](run-context.md).

---

## ChatSessionHandler Fields

| Field | Type | Purpose |
| --- | --- | --- |
| `session_id` | `i64` | DB session identifier |
| `db` | `Arc<SqlitePool>` | Persistent storage |
| `llm_manager` | `Arc<LlmManager>` | Resolves which LLM client to use |
| `max_history_messages` | `usize` | Max messages kept in context when compaction is disabled; ignored when compaction is configured |
| `max_tool_rounds` | `usize` | Max LLM rounds per turn before `Exhausted` |
| `max_tool_result_chars` | `Option<usize>` | When set, tool results from previous turns that exceed this char count are replaced with a placeholder in the LLM context. DB content is unchanged. See [Tool Result Hiding](#tool-result-hiding). |
| `agent_id` | `String` | Agent owning this session (default: `"main"`) |
| `tools` | `Arc<ToolRegistry>` | Built-in tools |
| `mcp` | `Arc<McpManager>` | MCP tools |
| `approval` | `Arc<ApprovalManager>` | Central approval service (rules + pending registry) |
| `event_bus` | `Arc<ChatEventBus>` | Publishes completed turns (user + assistant) to the in-process event bus |
| `question_registry` | `Arc<Mutex<HashMap<i64, oneshot::Sender<String>>>>` | Pending `ask_user_clarification` channels |
| `processing` | `Mutex<()>` | Prevents concurrent `handle_message` / `resume_turn` calls |
| `current_cancel` | `std::sync::Mutex<CancellationToken>` | Cancellation scope for the in-flight turn. A fresh token is minted per user message / resume and a clone is threaded by value through the whole recursive call tree; `cancel()` cancels the stored token. Never reset mid-turn → a `/stop` is sticky across sub-agent recursion |

---

## build_agent_config

Private helper called by both `handle_message` and `resume_turn` to avoid duplicating the LLM-resolution and tool-assembly logic.

1. Load `meta.json` for the current `agent_id` (scope, strength).
2. Resolve LLM client key via `LlmManager::resolve(client_name, scope, strength)`.
3. Build `base_tool_defs`: built-in tools + `call_agent` + `update_scratchpad`. **MCP tools are no longer included here** — they are resolved dynamically in `all_tool_defs()` each round based on `active_mcp_grants`.
4. Load session MCP grants from `session_mcp_grants` DB → populate `active_mcp_grants`. If `enabled_mcp_servers` override is provided, merge those names in-memory without touching the DB.
5. Inject `activate_tools` as an `InterfaceTool` (session-scoped, `stack_id = None`). Skipped if `enabled_mcp_servers` override is active.
6. **RunContext system prompt injection**: read `RunContext.extra_system_prompt()` and append its result to `extra_system_dynamic` (the dynamic tail system message, injected after conversation history, not cached). If both the caller-provided `extra_system_dynamic` and the RunContext fragments are non-empty, they are joined with `"\n\n"`.
7. Return `AgentRunConfig { ..., mcp: Arc<McpManager>, active_mcp_grants }`.

---

## handle_message Flow

1. Acquire `processing` mutex (blocks if another message is being processed).
2. Mint a fresh `CancellationToken`, store it in `current_cancel`, and thread a clone by value through `run_agent_turn` (and the sub-agent recursion).
3. **Memory context** — call `memory_manager.query_context(session_id, user_message)` for **all** sessions (including cron and tic). If a string is returned it is stored as `extra_system_dynamic` — **not** merged into `extra_system_context`. It will be injected as a dynamic tail system message after the conversation history (see *Context Building*). Only the write path filters by `is_interactive`/`is_ephemeral`.
4. Call `build_agent_config(client_name, enabled_mcp_servers, extra_system_static, extra_system_dynamic, interface_tools)` → `AgentRunConfig`. This also calls `memory_manager.tools()` and stores them in `AgentRunConfig::memory_tools`.
5. Get or create the active `chat_sessions_stack` frame.
6. Check for orphaned user message (see below) and mark it `failed` if found.
7. Append the user message to `chat_history` (with `is_synthetic` and optional `metadata` persisted via `append_with_metadata`); capture the returned `user_message_id`. `metadata` (type `MessageMetadata` in `core-api`) carries file attachments for web/mobile/Telegram messages; `content` stays the clean typed text. For non-synthetic messages, emit a `UserMessage { message_id, content, attachments }` event right after the append — the telnet-style echo that makes the bubble appear (clients never render the message optimistically).
8. Call `resume_pending_tools(stack_id, &config, &tx)` — re-gates and executes any `pending` tool calls left from an interrupted session.
9. Call `run_agent_turn(stack_id, &config, &tx, pending_input)` and await outcome. `pending_input: Option<Arc<dyn PendingUserInput>>` is the source's inbox handle for [live mid-turn injection](#mid-turn-injection) — `Some` for interactive web/mobile turns, `None` for cron/tic.
10. On `Final`: send `Done` event (and `Truncated` if applicable); then publish **two events** to `ChatEventBus` — one `User` event (with `is_synthetic` from the caller) and one `Assistant` event (with all `tool_calls` collected during the turn).
11. On `Cancelled`: send `Error` event ("interrupted by user"); return `Err("Turn cancelled by user")`. Background runners (cron, tickets) see the `Err` and record the job as `"failed"`. The WS handler logs at INFO level (not ERROR) when it detects this error string, since the client already received the error event.
12. On `Exhausted`: send `Error` event (tool round limit exceeded); return `Err(...)`. Background runners (cron, tickets) see the `Err` and record the job as `"failed"`. Interactive WS sessions already received the `ServerEvent::Error`; the returned `Err` is logged by the WS handler.

`is_synthetic` is a parameter of `handle_message`. It is `true` for TicManager ticks (system-generated messages injected as user turns), `false` for all real user input. Additionally, `ChatHub::notification_consumer` injects synthetic **Assistant** messages with `is_synthetic = true` containing the `read_notification` tool call and reasoning trace — these are not user turns, but share the same flag for UI filtering. The flag is **persisted** to `chat_history.is_synthetic` so that the UI history API (`GET /api/sessions/:id`) can filter those rows out on page reload — synthetic messages never appear in the conversation visible to the user. They are still included in the LLM context (via `build_openai_messages`) so the assistant can see what it previously said in response to a notification.

### Session detail debug view

`GET /api/sessions/:id` returns the full session tree in a debug-friendly format. Unlike the live message API, this endpoint:

- **Includes** synthetic user messages (marked `is_synthetic: true` in the JSON)
- **Includes** `reasoning_content` on assistant / thinking messages
- Returns session metadata (`source`, `agent_id`, `is_interactive`, `is_ephemeral`, `created_at`)

Response shape:

```json
{
  "session": { "id": 42, "source": "tic", "agent_id": "main", "is_interactive": false, "is_ephemeral": true, "created_at": "…" },
  "messages": [
    { "kind": "user",      "content": "…", "is_synthetic": true,  "created_at": "…" },
    { "kind": "thinking",  "content": "…", "reasoning": "…|null", "created_at": "…", "input_tokens": N, "output_tokens": N },
    { "kind": "assistant", "content": "…", "reasoning": "…|null", "created_at": "…", "input_tokens": N, "output_tokens": N },
    { "kind": "tool",      "name": "…", "arguments": {}, "status": "done|error|pending", "result": "…" },
    { "kind": "agent",     "agent_id": "…", "depth": N },
    { "kind": "agent_end", "agent_id": "…", "depth": N }
  ]
}
```

The frontend `<session-detail-page>` renders this at hash `#session/{id}`. The detail page includes a **Back** button that calls `history.back()`. The page is not linked directly in the sidebar but is fully functional when the hash is set directly, or when navigated to from the TIC Sessions page.

### Session list API

`GET /api/sessions?source=tic&page=1&per_page=20` — paginated list of sessions, optionally filtered by source.

Response shape:

```json
{
  "items": [
    { "id": 42, "source": "tic", "agent_id": "main", "is_ephemeral": true, "is_interactive": false,
      "created_at": "…", "message_count": 7, "last_message_at": "…" }
  ],
  "total": 100, "page": 1, "per_page": 20
}
```

The `<tic-sessions-page>` component renders this at hash `#tic` (linked from the sidebar under **TIC Sessions**). Each row is clickable and navigates to `#session/{id}`.

---

## resume_turn Flow

Called by `ChatHub::resume()` (routed through the global event bus) when the client sends `{"type":"resume"}`, and by `inject_async_result` after an async task finishes. Continues without appending a new user message. It is **not** part of the normal synchronous sub-agent path (that is plain recursion in `dispatch_sub_agent`); `resume_turn` exists for app-restart recovery of an active child stack, async result injection, and the WS resume message.

1. Acquire `processing` mutex.
2. Mint a fresh `CancellationToken` (a resume is a new unit of work — it must not inherit a stale cancellation, but a `/stop` during the resume still cancels this token) and store it in `current_cancel`.
3. Call `build_agent_config(...)` → `AgentRunConfig`.
3b. **`reap_interrupted_parallel_batches`** — the linear cascade below assumes one active frame per depth, which an interrupted [parallel sub-agent batch](#parallel-sub-agent-batches) violates. Detect a batch by ≥2 active `chat_sessions_stack` frames sharing a depth (impossible for a linear stack), then — accepting the single-user tolerance for restart loss — fail each such frame's spawning tool call and terminate the frame, from the shallowest multi-frame depth downward. The parent is left with a fully-resolved tool-call set and the normal cascade resumes it. A lone interrupted sub-agent (one frame at its depth) is left untouched.
4. Get the active `chat_sessions_stack` frame — if none exists, return immediately.
5. Call `resume_pending_tools(stack_id)`.
6. **Seed the cascade**: if no pending tools were found AND the deepest active frame's last assistant message has no associated tool calls (pure-text final response), that frame's own turn is already complete. Two sub-cases:
   - **Root frame** (no `parent_tool_call_id`) → nothing to do, return immediately.
   - **Child frame** (has a `parent_tool_call_id`) → its result was produced but **never propagated** to the parent (e.g. the turn task died right after the child finished — see below). Seed `current_outcome` from the existing final message (its `content`) **without re-running the LLM**, and enter the cascade so the parent's tool call is completed and the parent continues. *(Skipping this case unconditionally — as an earlier version did — left the parent wedged forever: tool call stuck `running`, child frame never terminated, main agent never resumed.)*
   Otherwise (last assistant message *does* have tool calls, e.g. a `task_completed` injected asynchronously), call `run_agent_turn(stack_id, …, None)` — resume never does live user-message injection.
7. **Cascade loop**: while the current stack has a `parent_tool_call_id`, complete/fail the parent's tool call, terminate the child stack, and run `run_agent_turn` on the parent stack. Repeat until reaching the root (depth = 0). Handles both restart recovery of an active child stack and the "completed-child never propagated" repair seeded in step 6.
8. At root: same `Final` / `Cancelled` / `Exhausted` handling as `handle_message`.

---

## resume_pending_tools

Called at the start of `handle_message` (and by the REST endpoint after a manual resolve). Finds any `running`/`pending` tool calls left from a previous interrupted session, re-runs them through the approval gate, executes approved ones, and rejects denied ones — so `run_agent_turn` sees complete history and can continue cleanly. Takes the turn's `token` so a `/stop` during resume cancels cleanly.

It shares the same collaborators as the live loop: `run_approval_gate` (so resume applies the **RunContext fast-path** and the **auto-deny** short-circuit identically — previously only the live loop did) and `record_tool_outcome` (so `resume` does not accumulate `ToolCallEvent`s nor re-emit `FileChanged`, which are live-turn concerns — it passes `None` for both).

**Rehydration = re-run from intent.** Each pending row is `(name, args, status)`; the live future was never serialized. The tool is reconstructed with the same `build_execution(name, args) → ToolExecution` used by the live loop and re-run from the start via `drive_execution`. This uniformly covers registry / memory / image / interface / MCP tools (previously only memory + registry were handled). `cancelled`/`rejected` rows are terminal and are **not** re-run.

`restart` is handled as a special case: it marks the call `done` in the DB before calling `std::process::exit(-1)`.

---

## AgentFlowSignal

`AgentFlowSignal` (`src/core/session/handler/mod.rs`) is a typed `pub(super)` enum used by internal dispatch methods to communicate control-flow outcomes through `anyhow::Error` without sentinel structs:

| Variant | Emitted by | Handled in |
| --- | --- | --- |
| `QuestionChannelClosed` | `dispatch_ask_user_clarification` (WS dropped) | `llm_loop.rs` → returns `TurnOutcome::Cancelled`; `resume.rs` → aborts resume |

Dispatch checks it with a single `downcast_ref::<AgentFlowSignal>()`.

---

## run_agent_turn Inner Loop

Called recursively via `Box::pin` to support async recursion without stack overflow.

`run_agent_turn` is a **thin round orchestrator**: each round's real work is delegated to focused collaborators (each an `impl ChatSessionHandler` block in its own file under `src/core/session/handler/`), so the loop reads as a sequence of named steps rather than one large function. The collaborators are shared with `resume_pending_tools`, so a live turn and a resume gate/execute/record identically.

| Collaborator | File | Responsibility |
| --- | --- | --- |
| `TurnEmitter` | `emitter.rs` | Typed, fire-and-forget seam over the per-turn `mpsc::Sender<ServerEvent>` — one semantic method per event (`tool_done`, `thinking`, …). All turn events flow through it. |
| `call_llm_round` → `RoundLlm` | `llm_call.rs` | One LLM call for the round + automatic model fallback (retries, `ModelFallback`/`LlmFailed`, message rebuild on `prompt_cache` change). Mutates `cur_name`/`cur_llm`/`messages` in place. |
| `handle_tool_call` → `CallFlow` | `llm_loop.rs` | Handles one tool call end-to-end (persist row → `ToolStart` → gate → restart → dispatch → record). Returns `Continue` or `End(outcome)`. |
| `effective_args` | `dispatch.rs` | Applies the RunContext working directory to a call's args (relative `path` → absolute, inject `workdir` for `execute_cmd`). |
| `run_approval_gate` → `GateOutcome` | `gate.rs` | The approval decision + human-approval flow (see [Approval Gate](#approval-gate)). |
| `execute_tool_call` → `DispatchResult` | `dispatch.rs` | Routes an approved call to the right executor (special non-cancellable paths + the unified cancellable `ToolExecution` path). |
| `record_tool_outcome` → `RecordFlow` | `outcome.rs` | Persists a tool outcome and emits `ToolDone`/`ToolError`/`ToolCancelled`. |

Takes the per-turn `token: &CancellationToken` by value-clone from the caller, plus `pending_input: Option<&Arc<dyn PendingUserInput>>` (see [Mid-turn injection](#mid-turn-injection)). For each round (up to `max_tool_rounds`):

1. Check `token.is_cancelled()` — return `Cancelled` immediately if set.
1b. **Mid-turn injection**: if `pending_input` is `Some`, `drain_user()` and append each queued message as its own `user` row + emit a `UserMessage` echo. These rows are read by `build_openai_messages()` in this same round, so the model sees them immediately. `None` for sub-agents / resume / non-interactive runners.
2. `build_openai_messages()` — reconstruct full context from DB.
3. `call_llm_round(...)` — one LLM call wrapped in `tokio::select!` against `token.cancelled()` (a `/stop` aborts the in-flight request → `RoundLlm::Cancelled` → `Cancelled`), with automatic model fallback on retriable errors. Returns `RoundLlm::{Turn, Cancelled, Failed}`.
4. On `LlmTurn::Message` — persist assistant message, return `Final` (with all `tool_calls` accumulated across rounds).
5. On `LlmTurn::ToolCalls` — persist the assistant "thinking" message (emit `Thinking` if non-empty). If the response is a **homogeneous batch** — `≥2` calls that are *all* synchronous sub-agents (`is_sync_sub_agent`) — dispatch them concurrently via `handle_sub_agent_batch` (see [Parallel sub-agent batches](#parallel-sub-agent-batches)); otherwise fall back to the sequential loop below: for each call (checking `token.is_cancelled()` before each one) call `handle_tool_call`, which:
   - Records the tool call in `chat_llm_tools` (status: `pending`) and emits `ToolStart` (with original LLM-provided args, before WD injection).
   - Computes `effective_args` — **working directory injection**: if `RunContext.effective_working_dir()` is set, resolve relative `path` args to absolute and inject `workdir` into `execute_cmd` args (if the LLM didn't already set one).
   - Runs `run_approval_gate` on `effective_args` (see [Approval Gate](#approval-gate)). `GateOutcome::Rejected` → the gate already marked the row `rejected` and emitted `ToolRejected` → `CallFlow::Continue` (skip); `GateOutcome::ChannelClosed` → `CallFlow::End(Cancelled)`.
   - Handles `restart` inline (mark `done`, emit `ToolDone`, `libc::_exit(-1)`).
   - Dispatches via `execute_tool_call`. **Special, non-cancellable paths** return a plain `Result<String>`: sync sub-agent (`execute_task` mode=sync / `execute_subtask`) → `dispatch_sub_agent` (recursive, inline); `update_scratchpad`/`write_todos`; `ask_user_clarification` → emit `AgentQuestion`, await answer; `task_completed` stub. **Everything else** (built-in registry incl. `execute_cmd`, memory/image tools, MCP, interface tools) goes through the **unified cancellable path**: `build_execution(name, args) → ToolExecution`, driven by `drive_execution(exec, token)`. See [Tool execution lifecycle](tools.md#tool-execution-lifecycle). If the clarification WS channel closed while awaiting an answer, `execute_tool_call` returns `DispatchResult::AbortPending` → the tool stays `pending` (not recorded) and the turn ends `Cancelled` so resume re-asks it.
   - Records the outcome via `record_tool_outcome`: `Completed` → `ToolDone`, status `done` (+ `FileChanged` for file-write tools); `Failed` → `ToolError`, status `failed`; `Cancelled` (a `/stop` hit the tool mid-flight) → `ToolCancelled`, status `cancelled`, and `handle_tool_call` returns `CallFlow::End(Cancelled)`. The execution's `stop()` was called (e.g. dropping the work future kills an `execute_cmd` child via `kill_on_drop`), so the tool aborts **immediately** instead of running to completion.
6. Loop back — next round rebuilds context with tool results included.
7. If all rounds exhausted: return `Exhausted`.

A sync sub-agent runs via `dispatch_sub_agent`, which awaits `run_agent_turn` recursively in the **same task** (same `processing` lock, same `token` clone) and returns the child's result as the parent tool call's result. Because parent and child share the token, a `/stop` that cancels a running child also stops the parent at its next check — no `WaitingChild` / task-spawn / resume cascade involved.

### Parallel sub-agent batches

When a single assistant response emits **≥2** synchronous sub-agent calls and *nothing else*, `handle_sub_agent_batch` (in `llm_loop.rs`) runs them concurrently instead of one-by-one. It is a three-phase restructuring of the same seams `handle_tool_call` uses, so the sequential path is left untouched as the fallback for every other shape (a lone call, or any mix with regular/side-effecting tools).

- **Phase 1 (sequential, in call order):** `chat_llm_tools::append` allocates each call's row id and emits `ToolStart`. The row id is what the LLM uses to reconstruct tool-result order (`ORDER BY id ASC`), so allocating them up front — before any concurrency — is what preserves ordering regardless of which sub-agent finishes first.
- **Phase 2 (concurrent, bounded):** the approval gate + `execute_tool_call` (→ `dispatch_sub_agent`) for all calls run through `futures::stream::…buffer_unordered(max_parallel_subagents)` (default `4`, config `llm.max_parallel_subagents`, `1` = sequential). Each future writes only to its own child stack frame + tool_call_id and borrows `&self`/`config`/`token`/`tx` — no shared mutable state between siblings. The shared cancellation token means a `/stop` (or one cancelled sibling) stops the others.
- **Phase 3 (sequential, in call order):** `record_tool_outcome` persists each result and appends to `all_tool_calls` in the original call order, so completion order never leaks into history or the event stream.

Siblings share the session-scoped scratchpad blackboard: concurrent writes to the *same* key are last-writer-wins by design (write distinct keys to avoid clobbering). Restart recovery of an interrupted batch is handled by `reap_interrupted_parallel_batches` (see [resume_turn Flow](#resume_turn-flow)).

### Mid-turn injection

A user can send a message while a turn is still running, and the agent picks it up at its next round boundary — without `/stop` and without waiting for the whole turn to finish.

- `run_agent_turn` receives `pending_input: Option<&Arc<dyn PendingUserInput>>` (the source's inbox handle from `ChatHub`). It is `Some` **only** for the root interactive turn; sub-agents (`dispatch_sub_agent`), `resume_turn`, and non-interactive runners (cron, tic) pass `None`.
- At the top of each round (step 1b above), the turn drains the inbox and appends each queued message as its **own** `chat_history` `user` row, then emits a `UserMessage` event (telnet-style echo — see [frontend.md](frontend.md)). The round boundary is the only safe ordering point: the previous round's assistant message + tool results are all persisted, so a trailing `user` row is well-ordered.
- It does **not** interrupt the in-flight LLM call or tool, and does **not** reset the round budget. Messages that arrive after the turn's last boundary stay queued and seed the next turn.
- `MessageBuilder` later merges consecutive non-failed `user`/`agent` rows into one `role:user` (see *Context Building*), so several injected messages read as one clean user turn for the LLM while the DB keeps each message distinct.
- A `/stop` clears the inbox, so queued-but-not-yet-injected messages are dropped, never persisted, never echoed. See [chat-hub.md](chat-hub.md) for the inbox/consumer side.

---

## Approval Gate

The gate is `ApprovalManager.check(session_id, category, agent_id, source, tool_name, args)` → `GateResult`.

**Evaluation order:**

1. Hardcoded exception: file-write tools targeting a path that starts with `memory/` → `Allow` (always auto-approved).
2. Rules from the `approval_rules` table, sorted by `priority ASC` (lower = evaluated first). First match wins.
3. **Session bypass** (in-memory, not persisted): if the result would be `Require` and an active bypass exists for this `session_id` whose `scope` matches (All, Category, or McpServer), convert to `Allow`. `Deny` is never bypassed.
4. No match → `Allow` (default-open policy).

**Default rules** (seeded at startup if the table is empty):
`execute_cmd`, `restart`, `write_file`, `edit_file`, `insert_at_line`, `replace_lines` → `require`

**Session bypass** is activated by the **human** (not the LLM) from the **Agent Inbox** UI or via the REST endpoint. Each bypass entry targets a `BypassScope`:

| Scope | What it covers |
| ----- | -------------- |
| `All` | Every tool regardless of category |
| `Category(ToolCategory)` | Only tools with the given registered category (e.g. `Filesystem`, `Shell`) |
| `McpServer(String)` | Only tools from the named MCP server (matched by the `mcp__<server>__` prefix) |

The bypass state lives in `ApprovalManager::session_bypasses` (`Mutex<HashMap<i64, Vec<ApprovalBypass>>>`). `check()` receives `session_id`, `category`, and `tool_name`. Expired entries are pruned lazily on each `check()` call. All entries for a session are cleared when `cancel_for_session()` is called (WS disconnect). The state is **never persisted** — it is reset on app restart.

**`run_approval_gate` (`gate.rs`)** wraps the whole decision + human-approval flow and returns a `GateOutcome`, so `run_agent_turn` and `resume_pending_tools` gate identically. It first calls `ApprovalManager.check(...)`, then applies the **RunContext fast-path** (relax `Require` → `Allow` for a pre-authorized file read/write path; never overrides a `Deny`), then:

- `Allow` → `GateOutcome::Proceed`.
- `Deny` → mark tool call `rejected`, emit `ToolRejected`, `GateOutcome::Rejected`.
- `Require`:
  1. If the session has `auto_deny_approvals` set (headless runners that cannot answer, e.g. TIC), mark `rejected` + emit `ToolRejected` → `GateOutcome::Rejected` (no blocking).
  2. Otherwise mark the row `pending`, register a `oneshot` channel via `ApprovalManager.register(...)` → `(request_id, rx)`.
  3. Call `emit_approval_event(em, request_id, tool_call_id, name, args)` (emits through the [`TurnEmitter`](#run_agent_turn-inner-loop)), which selects the event type:
     - **file-write tools** (`write_file`, `edit_file`, `insert_at_line`, `replace_lines`): read current file + compute predicted result concurrently → `PendingWrite { old_content, new_content }`. Falls back to `ApprovalRequired` if the diff cannot be computed.
     - **`execute_cmd`**: `PendingWrite` with `path = "$ execute_cmd"`, `new_content = "$ <command>"`.
     - **`restart`**: `PendingWrite` with restart description.
     - **everything else**: `ApprovalRequired { tool_name, arguments }`.
  4. Await `rx`:
     - `Approved` → `GateOutcome::Proceed`.
     - `Rejected { note }` → mark tool call `rejected` with the reason, emit `ToolRejected` → `GateOutcome::Rejected`. The saved reason — including the user's justification — is surfaced to the LLM as the tool-result content on the next request (see [MessageBuilder](#context-building)). Every reject surface (copilot WS, Agent Inbox, REST `/sessions` and `/inbox`, mobile, Telegram) passes the **raw** user note; the canonical message string is built in one place by `ApprovalDecision::rejection_message(note)` → `"User rejected this tool call. Reason: <note>"` (or `"User rejected this tool call."` when the note is empty), so wording stays consistent and no surface-specific prefix leaks into the LLM context.
     - Channel closed (WS disconnected) → `GateOutcome::ChannelClosed`, which the caller maps to `TurnOutcome::Cancelled` (live) / aborts the resume.

---

## MessageBuilder

`build_openai_messages` is now a thin wrapper that delegates to `MessageBuilder` (`src/core/session/handler/message_builder.rs`). `MessageBuilder` is a self-contained struct with no reference to `ChatSessionHandler`:

```rust
pub struct MessageBuilder {
    pub pool:                  Arc<SqlitePool>,
    pub session_id:            i64,
    pub mcp:                   Arc<McpManager>,
    pub datetime_config:       DatetimeConfig,
    pub max_history_messages:  usize,
    pub max_tool_result_chars: Option<usize>,
    pub compactor:             Option<Arc<ContextCompactor>>,
}
```

This allows the message-building logic to be tested in isolation with an in-memory SQLite database (no full `ChatSessionHandler` required). `ChatSessionHandler::build_openai_messages` constructs a `MessageBuilder` from its own fields and delegates.

---

## Context Building

`build_openai_messages` (backed by `MessageBuilder::build`) assembles the message array in the following order, optimised for prefix KV caching:

### 1. Static system message

Contents: AGENT.md + `inject_memory` files + `extra_system_static` (e.g. Telegram format rules) + MCP list.

**Runtime substitutions**: after assembling the static content, `MessageBuilder::build` applies `system_substitutions` — each entry replaces the `__KEY__` sentinel with the provided value. These sentinels originate from `<!-- KEY -->` directives in AGENT.md (resolved by `agents::resolve_includes`).

When `cache_hints = true` (Anthropic models via OpenRouter), the content is wrapped in a `cache_control: ephemeral` block so the provider caches it as a KV prefix. For all other providers this message is a plain string that never changes turn-to-turn, so the provider's own automatic prefix cache (if any) hits on it.

### 2. Scratchpad system message *(if non-empty)*

The session scratchpad emitted as a separate `[system]` message **before** the conversation. Kept isolated from the static block so a mid-turn `update_scratchpad` call only invalidates this small message, not the large cacheable prefix.

**Async sub-tasks** share the parent session's scratchpad: when a task is launched with `kind='async'`, its handler is initialised with `scratchpad_session_id = parent_session_id`. All reads and writes via `update_scratchpad` are then scoped to the parent session instead of the task's own isolated session, so 5 parallel async tasks launched by the same parent all read/write the same shared pad.

### 3. Compaction summary system message *(if present)*

See *Context Compaction*.

### 4. Conversation history

`chat_history` for the stack. When compaction is **disabled**, the list is truncated to `max_history_messages` (oldest dropped first). When compaction is **enabled**, `max_history_messages` has no effect — the compactor owns the token budget and truncating by count would silently discard history that should be summarised instead. For a user/agent row that carries attachments in its `metadata` column, the builder appends an `[SYSTEM INFO]` block (`core_api::message_meta::attachments_block`, path-only) to that turn's content **on the fly** — the block is never persisted; `content` stays the clean typed text and the UI renders the same `metadata.attachments` as chips. **Consecutive non-failed `user`/`agent` rows are coalesced into a single `role:user` message** (their contents joined by blank lines, attachment blocks preserved) — `for_stack` already excludes `failed` rows. This is what keeps the LLM context clean when several messages were stored as distinct rows, e.g. injected back-to-back mid-turn (see [Mid-turn injection](#mid-turn-injection)). Each assistant entry with tool calls in `chat_llm_tools` is reconstructed with a `tool_calls` array and one `tool` result message per call. The tool-result content is derived from the call's terminal status:

| Status | LLM-visible `tool` content |
| ------ | -------------------------- |
| `done` | the saved `result` |
| `failed` | `Error: <result>` |
| `rejected` | the saved reason (e.g. `User rejected this tool call. Reason: <note>`) — the human's justification reaches the LLM verbatim |
| `cancelled` | the saved note (a `/stop` cancellation) |
| `pending` / `running` (interrupted by a crash or lost connection) | `Error: tool call was interrupted (connection lost before user approval). Please retry the operation.` |

Tool result hiding (see below) is applied to results from previous turns.

### 5. Dynamic tail system message

Contains `extra_system_dynamic` (e.g. Honcho long-term memories, retrieved fresh each turn) followed by a date/time/OS/working-directory block:

- **Date/time** — formatted in the effective timezone (the `datetime.timezone` config value if set, otherwise the OS timezone via `iana-time-zone`); the IANA name is shown alongside the offset, e.g. `2026-06-17T21:20:00+01:00 (Europe/Rome)`.
- **Operating system** — type + version via `os_info` (e.g. `Mac OS 15.5.0 [64-bit]`), computed once and cached.
- **Working directory** — the session's effective WD, followed by a note that filesystem tools and `execute_cmd` use it for relative paths (no need to `cd`).

Placed **after** the conversation so the stable prefix (messages 1–4) is never invalidated by per-turn changes. The model's recency-biased attention also ensures it reads fresh user context immediately before generating its response.

### 6. Tail reminder system message *(if provided)*

Short anti-drift reminder (e.g. Telegram HTML format rules) at the very end.

---

## Tool Result Hiding

Controlled by `max_tool_result_chars` in `config.yml` (`llm.max_tool_result_chars`).

When set, `build_openai_messages` calls `maybe_hide_tool_result` for every tool result it reconstructs. The replacement happens **only when all three conditions hold**:

1. The result belongs to a **previous turn** — i.e. the assistant message that produced it appears before the last user/agent message in the (truncated) history.
2. `max_tool_result_chars` is `Some(n)`.
3. The result string exceeds `n` characters.

When all three are true, the content sent to the LLM is replaced with:

```text
[Tool response for `<tool_name>` hidden: response was N chars, exceeding the L-char limit. Call the tool again if you need this information.]
```

**What is never affected:**

- The database row — always retains the original content.
- The frontend — always displays the full result.
- Tool results from the **current turn** — always shown in full, regardless of size, so the LLM can work with them within the same turn.

**Current-turn boundary detection:** the last `User` or `Agent` role entry in the truncated history marks the start of the current turn. Any assistant message at a lower index is from a previous turn.

### Scratchpad injection format

```xml
<scratchpad>
  <!-- Temporary notes shared by all agents in this session (including async sub-tasks). Not persisted across sessions. -->
  <note key="db_url">postgres://localhost/mydb</note>
  <note key="main_struct">src/session/handler/mod.rs</note>
</scratchpad>
```

Only injected when the `session_scratchpad` table has at least one row for the session. For async sub-tasks the `session_id` used here is the parent's (see above).

---

## TurnOutcome Enum

| Variant | Meaning |
| --- | --- |
| `Final { content, message_id, input_tokens, output_tokens, truncated, tool_calls }` | LLM produced a final text response; `tool_calls` carries all `ToolCallEvent`s from all rounds |
| `Cancelled` | The turn's `CancellationToken` was cancelled (`/stop`), or WS closed during approval. `handle_message` returns `Ok(())`. |
| `Exhausted` | All `max_tool_rounds` used without a final message. `handle_message` returns `Err(...)` so background runners record the job as `"failed"`. |

---

## Session Cancellation via System Bus

Forceful task termination (e.g. the kill-task API) goes through the system bus to avoid direct coupling between the HTTP layer and the session internals.

**Flow**:

1. `POST /api/cron/jobs/{id}/kill` reads `running_session_id` from the DB and emits `SystemEvent::SessionCancelled { session_id }` on the system bus. Returns 202 immediately.
2. A background subscriber started in `Skald::new()` receives the event and calls `ChatSessionManager::cancel_session(session_id)`.
3. `cancel_session` — operates only on handlers already in the `active` map (no side-effectful creation for an unknown session):
   - `handler.cancel()` — cancels the `CancellationToken`; LLM calls and `execute_cmd` unblock via `tokio::select!`.
   - `handler.cancel_pending_approvals()` — drops the `oneshot::Sender` for every pending approval of that session; `approve_rx.await` returns `Err`, which the loop interprets as `TurnOutcome::Cancelled`.
   - `handler.cancel_pending_questions()` — same for clarification channels; `rx.await` returns `Err(QuestionChannelClosed)`, which also yields `TurnOutcome::Cancelled`.
4. `handle_message` returns `Err("Turn cancelled by user")` → `run_job` records the job run as `"failed"`.

This means kill works correctly even when the task is blocked on `ask_user_clarification` or waiting for human approval — both unblock the moment `cancel_session` drops their sender channels.

---

## Concurrency Constraint

Only one `handle_message` / `resume_turn` call can run per `ChatSessionHandler` at a time. The `processing: Mutex<()>` is held for the entire duration. A second call blocks until the first completes or is cancelled.

Note that callers don't reach `handle_message` directly: `ChatHub` serializes user messages **per source** through a single-consumer inbox *before* the `processing` lock, and messages that arrive during an in-flight turn are **injected into that turn** at a round boundary (not queued as a separate turn). So in practice the `processing` lock is rarely contended for interactive sources — see [chat-hub.md](chat-hub.md).

Synchronous sub-agents run **inline in the same task** as the parent (plain recursion in `dispatch_sub_agent`), so the single `processing` lock covers the whole parent+child tree — one user message is one logical critical section. A [parallel sub-agent batch](#parallel-sub-agent-batches) still runs within that one critical section and one task: `handle_sub_agent_batch` drives the concurrent children on the current task's async runtime (via `buffer_unordered`, not `tokio::spawn`), so they share the same `processing` guard and cancellation token — the concurrency is between the children, not between top-level turns. (Asynchronous tasks — `execute_task` mode=async — are a separate mechanism: a new ephemeral session driven by the cron runner, whose result is later injected via `inject_async_result` → `resume_turn`.)

---

## Orphaned Message Handling

If the last message in history has `role = User` or `role = Agent` (no following assistant message), the previous turn was cancelled before the LLM responded. That message is marked `status = failed` and excluded from the context sent to the LLM, preventing user→assistant alternation errors.

---

## AgentRunConfig

Built once per `handle_message` call and passed by reference through the entire agent/sub-agent recursion.

| Field | Purpose |
| --- | --- |
| `agent_id` | ID of the current agent |
| `client_name` | Resolved LLM client key |
| `depth` | Recursion depth: 0 = root, 1+ = sub-agent |
| `base_tool_defs` | Built-in tool definitions only (no MCP — those come from `all_tool_defs()` dynamically) |
| `extra_system` | Optional extra system context (set to `None` for sub-agents) |
| `system_substitutions` | `HashMap<String, String>` — named substitutions applied to the system prompt at build time. Each entry replaces `__KEY__` sentinels in the prompt text. |
| `interface_tools` | Interface-specific tools. For sub-agents contains only `activate_tools`; all other interface tools are dropped |
| `memory_tools` | Memory backend tools (inherited by sub-agents) |
| `mcp` | `Arc<McpManager>` — used by `all_tool_defs()` to resolve MCP tools dynamically |
| `active_mcp_grants` | `Arc<RwLock<HashSet<String>>>` — granted tool groups. Holds MCP server names and/or the reserved keyword `"config"` (which unlocks `config_tool_defs`). Re-read on every round so `activate_tools` in round N makes tools visible in round N+1. Root: session-scoped (from `session_mcp_grants` DB). Sub-agents: stack-scoped (from `stack_mcp_grants` DB), starts empty |
| `config_tool_defs` | `Vec<Value>` — built-in `Config`-category tool defs (the lazy `config` group). Appended by `all_tool_defs()` only when `active_mcp_grants` contains `"config"`. Pre-filtered (interactive-only / approval) by `build_agent_config` |

### `all_tool_defs()` — dynamic group resolution

Called on every LLM round. Returns `base_tool_defs` + MCP tools for currently-granted servers (re-queried from `McpManager` using `active_mcp_grants`) + `config_tool_defs` when the `config` group is granted + memory tools + interface tools.

This means that calling `activate_tools` in round N makes those tools available to the LLM starting from round N+1 of the **same turn** — no cross-turn delay.

Uniqueness is guaranteed by construction, not by a dedup pass: built-in tools have distinct names by registry construction, MCP tools are namespaced `mcp__{server}__{tool}`, and sub-agent inheritance cannot re-introduce a name because `for_sub_agent()` strips the per-level augmentations it re-derives (see below).

### `for_sub_agent()`

Derives a child config: inherits `base_tool_defs` (after filtering), `memory_tools`, and `mcp`; starts with **empty** `active_mcp_grants`; clears `interface_tools`; increments `depth`.

It drops two categories from the inherited `base_tool_defs`:
- **`root_agent_only` tools** (registry flag) — never exposed to sub-agents.
- **Per-level augmentations** that the config builders re-derive for every agent: `ask_user_clarification` and `execute_subtask`. Stripping them here makes `dispatch_sub_agent` the **single owner** of sub-agent augmentation, so a name can never be both inherited and re-added — the OpenAI-compat APIs reject non-unique tool names with HTTP 400, and this eliminates that failure mode structurally (no dedup pass needed).

`dispatch_sub_agent` then:

1. Replaces the empty `active_mcp_grants` arc with one pre-populated from `stack_mcp_grants` DB (restart recovery).
2. Appends `sub_agents_only` tools, `ask_user_clarification`, and (below the depth limit) `execute_subtask` to `base_tool_defs` — each present exactly once, since the inherited copy was stripped by `for_sub_agent()`.
3. Injects `activate_tools` (stack-scoped, `stack_id = Some(child.id)`) as the only interface tool.

---

## ask_user_clarification Flow

Available to every agent except hidden `system` agents (e.g. TIC), at any depth.

1. An agent calls `ask_user_clarification(question)`.
2. `run_agent_turn` intercepts it before ToolRegistry dispatch.
3. A `oneshot` channel is registered in `question_registry` keyed by `request_id`.
4. `AgentQuestion { request_id, question }` event is emitted to the frontend.
5. Execution is suspended until the client sends `{"type":"answer_question","request_id":<N>,"answer":"..."}`.
6. `resolve_question()` unblocks the channel; the answer is returned as the tool result.
7. On WS disconnect: `cancel_pending_questions()` drops all senders, causing the await to return `Err`, which propagates as a tool error.

---

## WS Resume Event Routing

When the client sends `{"type":"resume"}`, the WS handler calls `ChatHub::resume(&source)` which:

1. Finds the session handler for `source`.
2. Spawns a task running `handler.resume_turn(...)` with an mpsc sender.
3. Bridges every event from that sender to the **global broadcast bus** (tagged with the session's `source`).

All WS connections for the same source (including newly reconnected ones) receive the events via their global bus subscription. This avoids the previous design where events went to a local mpsc channel and were silently lost if the client reconnected while `resume_turn` was in flight.

### Running-state on (re)connect

A turn runs on a detached task and survives a page reload (closing the WS just `return`s from the socket loop — it does **not** cancel the turn). To let a reloaded client restore its SEND→STOP button, the WS handler — right after subscribing to the global bus — sends a `TurnRunning { running }` event to that socket, where `running = ChatSessionHandler::is_processing()` (a `try_lock` on the `processing` mutex, held for the whole turn). Because the send happens after subscribing, a turn that finishes immediately after still delivers its `Done` via the bus, which resets the client's state. The client also flips to "running" on any live streaming event (`thinking` / `tool_start` / `agent_start` / `pending_write` / `approval_required`) as a fallback. Note: with synchronous sub-agents now running recursively, the `processing` lock is held continuously for the whole parent+child tree, so `is_processing()` is a reliable signal.

---

## When to Update This File

- `needs_approval()` rules change (new tool added, path exemption modified)
- The tool-calling loop gains new behavior (new event type, new cancellation path)
- `build_openai_messages` changes (new context injected, truncation logic modified)
- `AgentRunConfig` fields change
- `build_agent_config` changes (new default tool added, resolution logic modified)
