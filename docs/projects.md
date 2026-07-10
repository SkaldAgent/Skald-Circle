# Projects

Filesystem-linked **projects**, each a unit of work tied to a directory on disk. A project gives
agents a standing context (path, description, permissions) so they know what they're working on
without the user re-explaining it every time.

Two ways to work on a project:

1. **Tickets** — fire-and-forget background tasks (one agent run per ticket).
2. **Interactive chat** — a persistent conversation with the project's coordinator agent, which
   delegates to specialist sub-agents.

For the database schema (`projects`, `project_tickets`) see [database.md](database.md). This file
documents the subsystem behavior.

---

## Modules

| Path | Role |
| ---- | ---- |
| `src/core/projects/mod.rs` | `ProjectManager` — CRUD; free fn `build_runtime_run_context` |
| `src/core/projects/tickets.rs` | `ProjectTicketManager` — ticket CRUD + lifecycle |
| `src/core/db/projects.rs`, `src/core/db/project_tickets.rs` | DAOs |
| `src/frontend/api/projects.rs` | REST + chat-session endpoints |
| `web/components/projects/` | `<projects-page>`, `<project-list-section>`, `<project-board-section>` |

---

## RunContext: `build_runtime_run_context`

`projects::build_runtime_run_context(project, base) -> RunContext` is the single place that turns a
project into a runtime [`RunContext`](approval.md). It layers project-runtime fields over an
optional `base` RC (which carries only static config set at creation, e.g. `security_group`):

- `working_directory` ← `project.path` (always overwritten).
- `allow_fs_writes` ← extended with the project tree and `{skald_cwd}/data` (pre-authorizes writes
  there, so tool calls in those trees skip the approval gate).
- `system_prompt` ← project-context fragments prepended: a project header (name + description) and
  a hint pointing at the personal-data dir.

It is shared by **both** execution paths below, so a ticket job and an interactive chat see an
identical project context. Edit it in one place.

---

## Tickets (background)

A ticket is an individual work item with a `title`, `description`, `agent_id`, and optional
`run_context` (static config only). Lifecycle in `ProjectTicketManager`:

- `create` / `delete` / `reset` — CRUD; every mutation calls `db::projects::touch` so the list
  orders by recency.
- `start(ticket_id)` — resolves the base RC (ticket override → project default), calls
  `build_runtime_run_context`, then spawns a background job via `TaskManager.spawn_async_job` with
  `origin_ref = "PROJECT_TASK:{id}"`. The ticket row records the `job_id` and moves to running.
- Completion is event-driven: `start_listener` subscribes to the system bus and, on
  `SystemEvent::JobCompleted` whose `origin_ref` matches `PROJECT_TASK:`, calls `on_job_completed`
  to persist the result/error and final status.

The board UI (`web/components/projects/project-board.js`) renders tickets in a single scrollable
list divided into three sections:

- **Running** — active tickets (status `pending` or `in_progress`), in start order.
- **Todo** — pending tickets, sorted by `created_at` descending (newest first).
- **Completed** — done/failed tickets, sorted by `completed_at` descending (most recent first).

The LLM result of a done ticket is rendered as markdown. Failed tickets show raw error text.
The view polls every 5 s while any ticket is running. Each ticket links to its session.

---

## Interactive project chat

A persistent conversation about the project, driven by the **`project-coordinator`** agent (see
[agents.md](agents.md)). A project can be of **any kind** — software, but also travel, study,
writing, events, personal goals — and the coordinator adapts to its nature (read from the injected
project description). The user talks to one bot that already knows the project; it does everyday
planning and writing itself and delegates specialized work — research via `researcher`, or code via
`tech-lead`/`software-architect`/`software-engineer` — to sub-agents through `execute_task`.

**Project memory (`SKALD.md`).** The coordinator's `meta.json` declares `"inject_memory": ["$WD/SKALD.md"]`.
The `$WD` placeholder expands to the session's working directory (the project path), so a `SKALD.md`
placed in the project root is auto-loaded into the system prompt as a `<memory_file>` block — the
per-project analogue of how `main` loads `data/memory/*`. If the file doesn't exist yet, a
`(file not created yet)` placeholder is injected, which the coordinator can fill in via `write_file`.
See `inject_memory` in [agents.md](agents.md).

**Source.** The chat is bound to source id `project-{id}` in the `sources` table (hyphen, not `:`,
so it stays URL-safe in `/api/{source}/messages`). The session is **interactive and
non-ephemeral**, so it persists and is resumed on reopen — unlike the disposable per-client
sessions [`ChatHub`](architecture.md) normally manages.

**Provisioning.** `api::projects::provisioning_for_source(skald, source)` maps a source to its
`(agent_id, RunContext)`:

- `project-{id}` → (`project-coordinator`, `build_runtime_run_context(project, project.run_context)`)
- anything else → (`main`, `None`)

This single resolver is reused by both endpoints so open and reset never diverge:

| Endpoint | Effect |
| --- | --- |
| `POST /api/projects/{id}/session` | `ChatHub::provision_session(source, agent, rc, reset=false)` — open or resume; returns `{ source, session_id }` |
| `POST /api/sessions?source=project-{id}` | same with `reset=true` — recreates the session with the **coordinator** (not `main`) |

`provision_session` is the only entry point for the source→session mapping ChatHub owns; the RC is
persisted at session creation (via `ChatSessionManager::create_session`) so it's present before the
handler is built. Because the session is interactive, `execute_task` is auto-injected, giving the
coordinator sub-agent delegation for free.

**UI (desktop).** The desktop copilot shows browser-style tabs: `General` (the `web` source, always
present) plus one tab per open project chat. The board's **Open Chat** button `POST`s the session
endpoint, then dispatches a `project-chat-open` window event (`{source, label}`); the copilot
adds/focuses the tab and calls `ChatSession._switchSource(source)` to swap the live WebSocket.
Closing a project tab is UI-only — the session persists and can be reopened from the board.

**UI (mobile).** The mobile web app (`<mobile-app>`) has a **Projects** bottom-nav entry rendering
`<projects-page>` (list from `GET /api/projects`). Tapping a project `POST`s the same session
endpoint and emits a `project-open` event; the shell navigates to `#chat/project-{id}`, which its
hash router turns into the `<chat-page>` `source` prop `project-{id}` (→ `_switchSource`) with the
project name in the header. A back button returns to `#chat` (the main `mobile` session), and the
hash survives refresh. It reuses the **same** `project-{id}` session as the desktop, so a project
chat is continuous across desktop, mobile browser, and — since the native iOS shell renders this web
app over the relay — remote. See [frontend.md](frontend.md).

---

## When to update this file

- Changing the project/ticket lifecycle or `build_runtime_run_context`.
- Changing how project chats are provisioned, sourced, or surfaced in the UI.
- Schema changes go in [database.md](database.md); the coordinator agent in [agents.md](agents.md).
