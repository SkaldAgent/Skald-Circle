# Cron Jobs & Background Tasks

## TaskManager

`TaskManager` manages scheduled cron jobs and on-demand background tasks (sync and async). It uses `std::sync::OnceLock` to hold late-injected dependencies, breaking circular chains that would arise if they were required at construction time:

| Dependency | Injected via | Needed for |
|---|---|---|
| `ChatSessionManager` | `set_session()` | Creating ephemeral sessions per job run |
| `ChatHub` | `set_hub()` | Sending completion/failure notifications; injecting `execute_task` InterfaceTool |
| `Arc<TaskManager>` (self) | `set_self_arc()` | Passing self-reference into `run_job` for sub-task tools |

`TaskManager` also holds an `Arc<SystemEventBus>` (passed at construction time, not via OnceLock) used to publish `SystemEvent::JobCompleted` when a job finishes. `ProjectTicketManager` subscribes independently; `TaskManager` has no direct reference to it.

In `main.rs`:

1. `TaskManager::new(pool, tz, system_bus)` — created first, OnceLocks empty
2. `ChatSessionManager::new(...)` — created second
3. `cron.set_session(Arc::clone(&manager))` — fills first OnceLock
4. `cron.start()` — background tasks begin (tick every 30 s)
5. `ChatHub::new()` — created after cron starts
6. `cron.set_hub(Arc::clone(&chat_hub))` — fills second OnceLock
7. `cron.set_self_arc(Arc::clone(&cron))` — fills third OnceLock
8. `chat_hub.set_task_mgr(Arc::clone(&cron))` — hub holds ref for InterfaceTool injection

The cron tick loop first fires 30 s after `start()`, so all OnceLocks are guaranteed to be filled before any job dispatch.

---

## Background Tasks

`start()` spawns two independent background tasks:

### Scheduler Loop

- Ticks every **30 seconds**
- Calls `db::scheduled_jobs::list_due(pool, &Utc::now().to_rfc3339())`
- Any cron job with `enabled=1`, `next_run_at <= now`, and `running_session_id IS NULL` is returned
- Each due job is spawned as an independent `tokio::task` via `run_job()`

### Cleanup Loop

- Waits 15 s at startup, then runs hourly
- Calls `cleanup_expired_single_runs(pool)` for jobs that are single-run, disabled, and older than 7 days. To satisfy the FK constraints, it clears the dependents first: nulls `project_tickets.job_id` (whose FK has no `ON DELETE` action, so a ticket still pointing at the job would block the delete), then deletes the job's `job_runs` rows, then deletes the `scheduled_jobs` rows. Tickets keep their result/error — only the GC'd job pointer is dropped. The manual `scheduled_jobs::delete()` performs the same cascade.

---

## 7-Field Cron Expression Format

**Format**: `sec min hour dom month dow year`

This is the format of the [`cron`](https://crates.io/crates/cron) crate — **not** standard Unix crontab (which uses 5 fields without seconds or year).

| Field | Values |
|---|---|
| sec | 0–59 |
| min | 0–59 |
| hour | 0–23 |
| dom | 1–31 or `*` |
| month | 1–12 or `*` |
| dow | 0–6 (Sun=0) or `*` |
| year | 4-digit year or `*` |

Examples:

| Expression | Meaning |
|---|---|
| `0 0 9 * * * *` | Every day at 09:00:00 |
| `0 */30 * * * * *` | Every 30 minutes |
| `0 0 8 * * 1 *` | Every Monday at 08:00 |

The `execute_task` tool (mode=cron) validates the expression with `Schedule::from_str()` before saving.

---

## Timezone

Cron expressions are evaluated in the timezone configured under `timezone` in `config.yml` (top-level IANA name, e.g. `Europe/Rome`). When omitted, the server's system local timezone is used as fallback. The same setting also controls the timestamp injected into the LLM context each turn.

The timezone is loaded at startup, logged at `INFO` level, and passed into `TaskManager`. All three points where `next_run_at` is computed (`add_job`, `toggle_job`, `run_job`) use the same `next_fire(schedule, tz)` helper which converts the result to UTC before storing.

---

## `next_run_at` (pre-computed fire time)

Rather than a sliding look-back window, the scheduler uses a **pre-computed `next_run_at` timestamp** stored in the DB:

- Set at job creation (first upcoming fire time after now, in the configured timezone)
- Advanced to the next fire time after each successful run
- Cleared when a job is disabled
- Recalculated from the cron expression when `toggle_item` (kind=cron) re-enables a job

This means: a tick simply does `WHERE next_run_at <= now` — no expression evaluation in the hot path. A missed tick is automatically covered because `next_run_at` stays in the past until the job actually runs.

---

## `kind` Column (three modes)

`scheduled_jobs` has a `kind` column with three values:

| `kind` | Behavior |
| ------ | -------- |
| `cron` | Scheduled job with a 7-field cron expression. Picked up by the tick loop when `next_run_at` is due. Result notified via `ChatHub::notify` (home conversation). |
| `sync` | Runs immediately on creation. No cron expression, no `next_run_at`. `single_run` is always true. Caller blocks until the agent finishes and receives the result inline. |
| `async` | Runs immediately in the background. Returns `task_id` immediately. When the agent finishes, the result is injected into the parent session as a synthetic message (see [Async Result Delivery](#async-result-delivery)). |

The `list_due()` query filters by `kind = 'cron'`, so sync/async tasks are never picked up by the scheduler tick loop. Recovery (`list_interrupted()`) applies to all kinds.

---

## `single_run` (one-shot jobs)

If `single_run=true`, after the first execution `finish_run()` receives `next_run_at=None`, which sets `enabled=0` (disabling the job) rather than advancing the schedule. The job stays in the DB as a disabled record and is purged after 7 days by the cleanup loop.

**Auto-detection for cron mode**: `add_job()` calls `next_fire_and_single()` which advances the cron iterator twice. If there is no second fire time — i.e. the expression can only ever match one point in time (e.g. `0 30 9 15 6 * 2026`) — `single_run` is forced to `true` regardless of what the caller passed. The LLM does not need to set `single_run` explicitly for specific-datetime expressions.

For `sync` and `async` modes, `single_run` is always `true` (they run once and are done).

---

## Job Lifecycle

### `cron` mode

1. LLM calls `execute_task(mode="cron", title, cron, prompt, agent_id)` → inserted in DB with `enabled=1`, `next_run_at` set to first upcoming fire time
2. Scheduler tick → `list_due()` returns the job
3. `run_job()` spawned (see below)
4. On completion: `hub.notify(...)` emits a structured completion notification (`source="cron"`) to the home conversation

### `sync` mode

1. LLM calls `execute_task(mode="sync", title, prompt, agent_id)` → LLM tool call blocks
2. `add_job_sync()` creates DB record and calls `run_job()` inside `block_in_place`
3. Agent runs to completion; final assistant message returned inline to the LLM tool call
4. Job marked disabled (single_run)

### `async` mode

1. LLM calls `execute_task(mode="async", title, prompt, agent_id)` → returns `task_id` immediately
2. `add_job_async()` creates DB record with `parent_session_id` set to the calling session and `run_context` (JSON blob) inherited from the parent
3. Agent spawned in background; LLM continues
4. On completion: `inject_async_result()` sends a synthetic message to the parent session

---

## `run_job` — execution core

`run_job(pool, session_mgr, task_mgr, hub, job, tz)` handles all three kinds:

1. New ephemeral session created (`is_ephemeral=1, is_interactive=0, source="cron"`)
2. `set_running(pool, job.id, session_id)` — marks job in-flight
3. If `job.run_context` is `Some`, stamps the RunContext JSON blob onto the new `chat_sessions` row directly before `get_or_create_handler()` loads it
4. `handler.set_context_label("CronJob: <title>")` — used for Agent Inbox labels
5. Job context injected via `extra_system_dynamic_override`
6. `execute_subtask` InterfaceTool injected, carrying the same `run_context` so nested subtasks also inherit it (see [Background Tool Restrictions](#background-tool-restrictions))
7. `tokio::spawn(handler.handle_message(...))` + concurrent drain of the event channel (prevents deadlock when the buffer fills)
8. After completion: delivery branch on `job.kind` (notify / return inline / inject_async_result)
9. `record_job_run()` writes to `job_runs` audit trail
10. `finish_run()` advances `next_run_at` for cron jobs; disables single-run jobs
11. Publishes `SystemEvent::JobCompleted { job_id, origin_ref, result, error }` on `system_bus`; `ProjectTicketManager` receives this event via its `start_listener()` background task and updates the ticket state when `origin_ref` starts with `"PROJECT_TASK:"`

On failure: error logged, job_runs row recorded with status `"failed"`, `hub.notify(...)` sends an error notification.

### Deadlock prevention

`handle_message` sends `ServerEvent` values into a bounded channel. If the caller drains only _after_ `handle_message` returns, the channel buffer can fill (especially with long agent chains) causing a deadlock. Fix: `handle_message` is spawned via `tokio::spawn`, and the calling task drains the channel concurrently. The `JoinHandle` is awaited after the channel closes.

---

## Async Result Delivery

When a `kind="async"` job completes, `inject_async_result()` follows the same pattern as the notification system:

1. Resolves the `source_id` via `chat_sessions::find_by_id(pool, parent_session_id)` → `session.source`
2. Gets the active stack for the parent session via `chat_sessions_stack::active_for_session`
3. Writes a synthetic **assistant message** (reasoning trace) directly to `chat_history`
4. Writes a completed **`task_completed` tool call** to `chat_llm_tools`, carrying `{task_id, title, result}` as the tool result payload
5. Calls `hub.resume(source_id)` — this bridges events to the global WebSocket bus and runs the LLM loop, which sees the completed tool call and responds

The delivery happens inside `run_job` (not in the `add_job_async` spawn closure) so the recovery path also calls it correctly.

**Note:** if the parent session has been cleared (`/clear`) the result is still injected — the session's history starts fresh but the notification arrives. This is intentional: the parent session ID is the correct semantic target even after a clear.

---

## Background Tool Restrictions

Background sessions (`kind="cron"` or `kind="async"`) **cannot** call `execute_task`. They receive `execute_subtask` instead:

| Tool | Available in | Notes |
| ---- | ------------ | ----- |
| `execute_task` | Interactive sessions only | Injected as InterfaceTool by `ChatHub::send_message`; `session_id` and `run_context` (JSON blob) captured in closure at tool-build time |
| `execute_subtask` | Background sessions only | Sync-only; no `mode` field; calls `add_job_sync()` internally; `run_context` propagated from the parent job |

This rule eliminates the complexity of tracking nested async/cron task lifecycles. Background tasks can spawn synchronous sub-work (via `execute_subtask` or via `call_agent`) but cannot launch new fire-and-forget or cron tasks.

---

## `running_session_id` (restart recovery)

`scheduled_jobs.running_session_id` is non-null while a job is in-flight. On restart:

1. `recover_interrupted()` runs once, before the first tick
2. Queries `list_interrupted()` — all jobs where `running_session_id IS NOT NULL`
3. For each interrupted job, `run_job()` is spawned again (creates a fresh session — the old one is abandoned)
4. For async jobs, `inject_async_result()` is called when the re-run completes

`list_due()` excludes rows with `running_session_id IS NOT NULL`, preventing double-runs.

---

## Session Handling

Each run always creates a **new ephemeral session**:

| Property | Value |
|---|---|
| `source` | `"cron"` |
| `is_interactive` | `0` |
| `is_ephemeral` | `1` |
| `agent_id` | job's `agent_id` (required at creation; must be a `task` agent) |
| `run_context` | inherited from `scheduled_jobs.run_context` JSON blob (may be null → falls back to the implicit `"default"` group) |

Sessions are not reused across runs. Each run gets a fresh context.

---

## RunContext Inheritance

Every task inherits the RunContext of the session that created it. This controls which tool-permission group the task runs under (tool visibility, approval rules).

**Inheritance chain:**

1. The parent interactive session has a `run_context` JSON blob (set by the user via the API; `None` otherwise)
2. `ChatHub::send_message` reads `handler.run_context_json()` **before** building the `execute_task` InterfaceTool and captures the value in the closure
3. `execute_with_session()` passes `run_context` to `add_job / add_job_sync / add_job_async`, which store it in `scheduled_jobs.run_context`
4. `run_job()` stamps the JSON blob onto the ephemeral child session before `get_or_create_handler()` loads the session — the manager's resolution path (`session.run_context` → `RunContext::from_db`) picks it up automatically
5. `execute_subtask` also captures `run_context`, so nested synchronous sub-tasks inherit it transitively

For **project tickets**, `ProjectTicketManager.start()` resolves the `run_context` itself (ticket override → project default) and passes it to `TaskManager.spawn_async_job()`.

**Override via Tasks UI:** the `PATCH /api/cron/jobs/{id}/run-context` endpoint allows overriding `scheduled_jobs.run_context` after creation. The dropdown in the Tasks page calls this endpoint.

---

## Agent Interaction

Jobs run via the `task` agent named in `agent_id` (required — no default; see [agents.md](agents.md)). `TaskManager` rejects an empty or non-`task` agent at creation. A typical task agent:

- Executes the task described in the cron prompt
- Delegates complex work to sub-agents (software-engineer, researcher, software-architect) via `execute_subtask`
- Calls `ask_user_clarification` when genuinely uncertain — this creates a pending entry in the `ClarificationManager` (visible in Agent Inbox) rather than blocking
- Its final assistant message is captured for delivery (notification / inline result / async injection)

---

## `job_runs` (audit trail)

Every execution is recorded in `db::job_runs`. Schema: see [database.md](database.md).

---

## LLM Tools for Tasks

| Tool | Availability | Action |
| ---- | ------------ | ------ |
| `execute_task` | Interactive sessions (web, telegram) | Create and run a task — cron/sync/async modes; validates cron expression; auto-detects single_run |
| `execute_subtask` | Background sessions only | Run a sync sub-task; blocks until complete; returns result inline |
| `read_agent_result` | Interactive sessions | Poll stub — always returns `not_ready`; real delivery is via synthetic message |
| `list_items` (type=cron) | All sessions | Returns JSON array of all tasks (id, title, cron, enabled, kind, next_run_at, single_run, last_run_at) |
| `delete_cron_job` | All sessions | Permanently deletes task by id |
| `toggle_item` (kind=cron) | All sessions | Enables or disables a task; recalculates next_run_at when re-enabling |

---

## When to Update This File

- Scheduler tick interval changes
- `next_run_at` / `list_due` logic changes
- `run_job` session-handling logic changes
- New task-related tools are added
- Recovery or cleanup loop logic changes
- Async delivery mechanism changes
