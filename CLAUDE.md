
# Skald (project-family) — codebase guide

Rust async web app (Tokio + Axum). Runs as a local chat server with LLM tool-calling and a sub-agent system.

## What this repository is

A **dedicated fork** of Skald, turning a single-user personal agent into a **multi-user assistant for a small trusted group** — positioned at families, but see the neutrality rule below.

The design lives in **`blueprint/project-family.md`**. Read it before any architectural work; its sections are referenced by number (§0.1 neutrality, §5.1 database layout, §11 `UserManager`, §12 auth schema, §16 LLM privacy tiers, §17 sequencing). The `blueprint/` directory is **gitignored and not under version control** — treat it as the source of truth, and never assume a section says what you remember.

Load-bearing decisions from that document:

- **Not upstreamable.** Nothing here needs to preserve Skald's schema or be portable back to it.
- **Greenfield.** No users in production ⇒ **no migrations, no backwards compatibility**. Tables get restructured, renamed and moved freely; the schema collapses into a single clean baseline v1.
- **Dual memory**: a private per-user pool plus a shared pool. A user's private space is encrypted so that nobody else — the admin included — can read it *through normal use of the system*. Never claim "mathematically impossible": the honest promise is transparency plus verifiability (§3).
- **Threat model** (§2): the adversary is the **tempted admin**, who owns the box but does not recompile the binary or dump RAM. Do not design against a forensic attacker.
- **Roles are data, not enums** (§0.1): a `roles` table binds permission-group, run-context and data-handling attributes. "Children" is a seeded preset row, never a hardcoded type.

### The core is domain-neutral — this is a hard rule

"Family" is **positioning, not architecture**. Schema, engine, API, identifiers **and comments** must never contain `family`, `household`, `parent`, `child` or `minor`. A pivot to teams, small orgs or care settings must not require renaming anything.

| Domain concept | Technical primitive |
| ---- | ---- |
| the group | **implicit — it is the instance**. No group entity. Future multi-group ⇒ `tenant` / `workspace`, never `family` |
| shared memory | `memory/shared` |
| parent / admin | role `admin` |
| child / minor | a **data-driven role** defined by the admin |
| "the parent reads the child's data" | a generic **supervision edge** between users |

Domain words are allowed only in seed data, preset labels, UI copy and positioning.

### Current state

Single-user Skald plus a `users` table (`src/core/db/users.rs`). Next step is `UserManager` (§11). The multi-user split of the database has not started; `Runtime` still carries one shared `Arc<SqlitePool>`.

Direction of travel, decided but not yet executed: strip the **power-user surface** (self-rewriting, arbitrary shell, dev-agent suite, ticket system) and move to a **binary-first** layout — the app is built once and run from a compiled binary, not executed from its own source tree.

## Key modules

| Path | Role |
| ---- | ---- |
| `src/main.rs` | Thin entry point: tracing → `Skald::new` → `WebFrontend::start` → shutdown. Branches on the `desktop` feature: under `--features desktop` enters `desktop::run()` (Tauri event loop) instead of blocking on a tokio runtime. Exposes `run_backend()` / `shutdown_backend()` shared by both entry points |
| `src/desktop/mod.rs` | Tauri shell — **only compiled under `--features desktop`**. Builds the system-tray icon + menu (`Open` / `Quit`), creates the main `WebviewWindow` (URL = `http://127.0.0.1:{config.port}`), spawns the backend on Tauri's shared tokio runtime, handles graceful shutdown. Holds the `OnceLock<AppHandle>` that the `restart` tool reads from. See [docs/desktop.md](docs/desktop.md) |
| `src/core/skald/` | `Skald` — headless application core. `mod.rs` (struct + staged `new()` / `shutdown()`), `runtime.rs` (cross-cutting `Runtime` context), `bundles.rs` (8 domain bundles + `build()`), `wiring.rs` (`wire()` + `spawn_background()`), `supervisor.rs` (`TaskSupervisor`), `accessors.rs` (per-manager accessor facade — the API surface the frontend uses) |
| `src/core/session/handler/` | Core LLM loop — `mod.rs`, `llm_loop.rs` (`run_agent_turn`), `agent_dispatch.rs`, `dispatcher.rs`, `approval.rs`, `resume.rs`, `messages.rs`, `config.rs`, `interface_tools.rs` |
| `src/core/session/manager.rs` | Creates/retrieves `ChatSessionHandler` per session |
| `src/core/chat_hub/` | `ChatHub`: broadcast events to all connected WS clients |
| `src/core/chat_event_bus.rs` | Global async bus for cross-session events |
| `src/core/agents.rs` | Discovers agents from `agents/*/`, loads meta + system prompt |
| `src/core/tools/` | Built-in tools: `exec`, `restart`, `list_agents`, `fs/*`, `notify`, `ast_outline`, `image_generate`, MCP tools, plugin tools, cron tools |
| `src/core/tool_catalog.rs` | `ToolCatalog`: unified tool listing façade (wraps ToolRegistry + McpManager) |
| `src/core/events.rs` | `ServerEvent` enum streamed over WebSocket to the frontend |
| `src/core/db/` | sqlx SQLite — see below |
| `src/config.rs` | Loads `config.yml`; LLM clients, strength/use_cases, data root. Also hosts `bootstrap_data_dir()` — under the `desktop` feature, relocates the process cwd to a per-user data dir when running inside a `.app` bundle (no-op in dev mode and headless mode) |
| `src/core/mcp/` | MCP client manager (connects to external MCP servers) |
| `src/core/plugin/` | Plugin system: discovery, enable/disable, tool registration |
| `src/core/cron/` | Scheduled job runner |
| `src/core/compactor.rs` | Context compaction (summarises history when token budget exceeded) |
| `src/core/approval/` | Approval rules engine |
| `src/core/clarification/` | `ClarificationManager`: background-session question/answer |
| `src/core/elicitation/` | `ElicitationManager` + bridge: MCP server-initiated input (`elicitation/create`), surfaced in the Inbox; secrets never logged/persisted |
| `src/core/inbox.rs` | `Inbox`: unified façade for pending approvals + clarifications + elicitations (wraps ApprovalManager, ClarificationManager, ElicitationManager) |
| `src/core/llm/` | LLM client abstraction (OpenAI-compat, Anthropic, Ollama…) |
| `src/core/transcribe/` | Transcription providers |
| `src/core/image_generate/` | Image generation providers |
| `src/core/memory/` | Agent memory tools |
| `src/frontend/mod.rs` | `WebFrontend`: wires router_factory, starts plugins, runs Axum |
| `src/frontend/server.rs` | Axum router, static file serving |
| `src/frontend/api/` | HTTP + WebSocket handlers — `State<Arc<Skald>>` |
| `web/components/` | Lit web components (see below) |

## DB tables (sqlx SQLite)

One file, `database/system.db` — the path is a constant (`core::db::SYSTEM_DB_PATH`), **not** configurable. `init_pool` creates the directory; SQLite only creates the file. Per-user `database/{userid}.db` files are the next step (§5.1).

`users`, `chat_sessions`, `chat_sessions_stack`, `chat_history`, `chat_llm_tools`, `chat_summaries`, `llm_requests`, `llm_providers`, `llm_models`, `transcribe_models`, `tts_models`, `image_generate_models`, `scheduled_jobs`, `job_runs`, `mcp_servers`, `mcp_events`, `plugins`, `approval_rules`, `tool_permission_groups`, `sources`, `secrets`, `config`, `session_scratchpad`, `session_mcp_grants`, `stack_mcp_grants`, `known_tools`, `projects`, `project_tickets`

`users` (`src/core/db/users.rs`) holds the directory plus auth material. It lives in the system DB, which the box owner can read, so it must never store anything that derives a user's key. `Credentials` is an enum mirroring the table's `CHECK`: an encrypted user carries a **wrapped DEK** (whose AEAD tag *is* the password verifier — hence no `password_hash`); a cleartext user carries an ordinary verifier, or none. `User` is deliberately not `Serialize` and its `Debug` redacts key material — use `User::summary()` for anything leaving the process. `role_id` has no foreign key yet: sqlx enables `PRAGMA foreign_keys`, so referencing the not-yet-existing `roles` table would fail every insert.

## Sub-agent system

- Synchronous sub-agents (`execute_task` mode=sync / `execute_subtask`) are **not** plain `Tool`s — they are intercepted in `run_agent_turn` before registry dispatch.
- `dispatch_sub_agent` (in `agent_dispatch.rs`) creates a child `chat_sessions_stack` row and runs `run_agent_turn` **recursively in the same task**, holding the same `processing` lock and sharing the same cancellation token. The child's result string becomes the parent tool call's result (completion lives in one place — the `run_agent_turn` tool-result match); then it terminates the child frame. There is no task-spawn / `WaitingChild` / resume cascade for the sync path.
- Max recursion depth: `MAX_AGENT_DEPTH = 5`.
- **Parallel batches:** when a single assistant response emits **≥2** sync sub-agent calls and *nothing else*, `run_agent_turn` fans them out concurrently via `handle_sub_agent_batch` (bounded by `max_parallel_subagents`, default `4`). Ordering is preserved by allocating every `chat_llm_tools` row up front in call order (the LLM reconstructs results by row id), then recording outcomes back in call order; only the middle dispatch is concurrent. Any other shape (a lone call, or a mix with regular tools) keeps the strictly sequential `handle_tool_call` loop — the two paths share the same lower-level seams. Siblings share the session's scratchpad blackboard (session-keyed): concurrent writes to the *same* key are last-writer-wins by design.
- **Restart recovery of a parallel batch** is intentionally lossy (single-user app): `resume_turn` first calls `reap_interrupted_parallel_batches`, which detects a batch by ≥2 active `chat_sessions_stack` frames at the same depth (impossible for a linear stack), fails their spawning tool calls and terminates the frames, then lets the normal linear cascade resume the parent. A lone interrupted sub-agent is untouched and still recovers via the cascade.
- Client resolution order: `args.client` → `meta.json client` → AUTO selection by scope/strength.
- **The parent's resolved client is NOT inherited.** Passing a concrete model name to `resolve()` bypasses strength/scope checks; sub-agents always auto-select unless overridden explicitly.
- `list_agents` is a plain tool; returns JSON excluding `main`.
- `resume_turn` (+ its cascade) is kept only for: app-restart recovery of an active child stack, async task result injection (`inject_async_result`), and the WS resume message — not for the normal sync dispatch.

## Cancellation (stop)

- Each turn has a `CancellationToken` (`tokio_util`). `handle_message` mints a fresh one per user message and stores it in `current_cancel`; `resume_turn` mints one per resume. A **clone is threaded by value** through the whole (recursive) call tree — never re-read from the field mid-turn — so a `/stop` is **sticky** across sub-agent recursion.
- `cancel()` cancels the stored token. It is checked at each round boundary and before each tool call, wrapped around the in-flight LLM call (`tokio::select!`, aborting the request), and wrapped around `execute_cmd` (drops the future → `kill_on_drop` kills the shell process). Parent and child share the token, so a cancelled child stops the parent by construction.

## Approval gate

The rule engine `ApprovalManager::check` returns `Allow`/`Deny`/`Require` per tool call (default rules seeded on first boot; the catch-all `* require @999999` gates anything not explicitly allowed — e.g. `execute_cmd`, `restart`, `execute_task`, writes outside whitelisted paths). A `Require` registers a `oneshot` in the in-memory `pending` map keyed by `request_id` and emits an approval event over WS.

Resolution is **source-agnostic**: the WS + Inbox paths resolve by `request_id`; the inline chat card resolves by the durable `tool_call_id` via `POST /api/tools/:tool_call_id/resolve` (`resolve_tool` in `src/frontend/api/sessions.rs`), which derives the owning session from the tool call's own stack row — never a hardcoded source. Live pending cards fire the `oneshot`; post-restart they execute directly on the owning session. See `docs/approval/`.

**Tool visibility in the Security-groups UI** (`GET /api/approval/tools`): tools injected outside the `ToolRegistry` (interface/plugin/provider tools) would otherwise be un-configurable. `ToolCatalog::list_all()` covers registry tools + a static `synthetic_tools()` list of core interface tools; everything else is captured by `src/core/tool_discovery.rs` (`ToolDiscovery`), which taps `all_tool_defs()` in `llm_loop.rs` each round and upserts every offered tool into the `known_tools` table (in-memory seen-set guard → background DB write). `list_tools` merges `known_tools` (deduped, `category: "dynamic"`) so any tool offered at least once becomes gate-able. Drift-proof by construction; core never hardcodes plugin tool names.

## Restart

`restart` **no longer rebuilds anything** — neither mode compiles.

- **Headless** (default): `restart` calls `libc::_exit(-1)` (= exit code 255); `run.sh` re-executes the same binary *by path*.
- **Desktop** (`--features desktop`): Tauri cleanup + respawn of the bundled binary + `exit(0)`.

Use it to pick up `config.yml` / database changes, which are only read at startup. To load new **code**: `./build.sh`, then restart — the supervisor picks up the new binary on the next loop, since `build.sh` installs it with an atomic rename.

> `restart`'s tool description in `src/core/tools/restart.rs` still tells the model it triggers a `cargo build`. That is stale and must be fixed, along with `run.bat` (still `cargo run`).

## Build & run

```sh
./build.sh      # cargo build --release → bin/skald (atomic install)
./build.sh -d   # debug profile; extra args are forwarded to cargo
./run.sh        # supervisor loop over the pre-built binary — never compiles
```

`run.sh` resolves the binary as `$SKALD_BIN` → `bin/skald` → `target/release/skald`, and warns when sources are newer than the binary. Exit `0` stops the loop, `255` re-executes, anything else propagates.

Tracing filter: `RUST_LOG=skald=debug,info`

### Desktop bundle (Tauri)

```sh
cargo run --features desktop          # dev: real window + tray, no bundle
cargo tauri build --features desktop  # release bundle: .app / .exe / .AppImage
```

Requires `cargo install tauri-cli --version "^2"`. The `desktop` feature is default-off.

## Adding an agent

Create `agents/<id>/meta.json` and `agents/<id>/AGENT.md`. The agent is discovered at runtime (no restart needed for prompt edits). Optionally set `"client": "<name>"` in meta.json to pin a specific LLM.

## Documentation

The `docs/` directory is **ignored** for now — do not read it, reference it, or update it. It is slated for removal.

## Config

Copy `default.config.yaml` → `config.yml`. Never commit `config.yml` (contains API keys).

## Python environment

All Python scripts (MCP servers, setup scripts) use a local virtualenv at `.venv/` in the project root.

`run.sh` creates it automatically on first launch (using `uv` if available, otherwise `python3 -m venv`) and installs `requirements.txt`. It then prepends `.venv/bin` to `PATH` before starting the app, so every child process — MCP server launches, `execute_cmd` shell calls — resolves `python3` to the venv automatically. No manual activation needed. **Python is optional**: if neither `uv` nor `python3` is found, the app starts normally and only Python-based MCP servers will be unavailable.

To add a Python dependency: add it to `requirements.txt`. It will be installed on the next `./run.sh` invocation if `.venv` does not yet exist — or run `uv pip install -r requirements.txt` manually.

## Frontend components (`web/components/`)

All extend `LightElement` from `web/lib/base.js` (Lit). `ChatSession` (`web/lib/chat-session.js`) is the shared base for WS-connected chat UIs.

| File | Element | Notes |
| ---- | ------- | ----- |
| `copilot.js` | `<app-copilot>` | Desktop copilot (`_wsSource='web'`); composer input with model pill, auto-resize textarea |
| `shared/chat-page.js` | `<chat-page>` | Mobile chat (`_wsSource='mobile'`) |
| `copilot-render.js` | (helpers) | `renderMsg`, `renderTool`, `renderDiff`, etc. — shared by copilot and chat-page |
| `sidebar.js` | `<app-sidebar>` | Nav sidebar; polls `/api/inbox` every 10 s for badge |
| `topbar.js` | `<app-topbar>` | Top nav bar |
| `home-page.js` | `<home-page>` | Landing / dashboard |
| `shared/file-viewer-base.js` | `FileViewerBase` (base) | Shared file-viewer engine (fetch, kind detection, markdown/PDF/SVG/LaTeX, watcher, `_renderBody`); driven by `_show`/`_hide`. Extended by desktop + mobile |
| `file-viewer-page.js` | `<file-viewer-page>` | Desktop file viewer: `FileViewerBase` + hash routing via `window.openFile(path)` → `#file_viewer?path=...` |
| `shared/file-viewer-mobile.js` | `<mobile-file-viewer-page>` | Mobile file viewer: `FileViewerBase` + prop-driven (`visible`/`path`), full-screen with back button |
| `agents.js` | `<agents-page>` | Agent discovery and config |
| `agent-inbox.js` | `<agent-inbox-page>` | Pending approvals + clarifications from background sessions |
| `approval-rules.js` | `<approval-rules-page>` | Approval rule management |
| `cron-jobs.js` | `<cron-jobs-page>` | Scheduled job management |
| `llm-providers.js` | `<llm-providers-page>` | LLM provider management |
| `models-hub.js` | `<models-hub-page>` | Models hub landing (LLM / Transcription / Image) |
| `models-llm.js` | `<models-llm-section>` | LLM model CRUD + drag-and-drop priority |
| `models-transcribe.js` | `<models-transcribe-section>` | Transcription model CRUD |
| `models-image.js` | `<models-image-section>` | Image generation model CRUD |
| `mobile-app.js` | `<mobile-app>` | Mobile app shell |
