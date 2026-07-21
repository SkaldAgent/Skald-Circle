
# Skald (project-family) — codebase guide

Rust async web app (Tokio + Axum). Runs as a local chat server with LLM tool-calling and a sub-agent system.

> **Never `git commit` unless explicitly asked.** Staging, building, running and testing are fine on your own initiative; creating a commit is not. Do the work, leave it in the working tree, and let the user commit — or ask them to — even when a commit looks like the obvious next step.
>
> **Commit messages must be in English.**

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

`UserManager` (§11) is now **consumed**. Login exists (`crates/skald-core/src/auth/`: `SessionStore` + the `guard.rs` deny-by-default middleware; first admin created by `skald-setup`), and the per-user owner-bound runtime is `UserContext` (`crates/skald-core/src/skald/user_context.rs`) — resolved by `Skald::user_context` / the frontend's `require_context`, keyed off `UserManager::pool_of`. The frontend owner call-sites (WS, sessions, inbox, approval-pending, projects, uploads, run-context, **cron**) route through the per-user pool; dev/stats read `llm_requests` — a *registry* table — from `system.db`, which is correct. The "owner-without-a-user" question resolved to **there isn't one**: every owner content belongs to a logged-in user (the admin included). The global owner-bound bundles (`Conversation`/`Tasks`: the "ownerless" `ChatSessionManager`, `ChatHub`, cron `TaskManager`, `TicManager`) are still constructed but **inert** — their loops never spawn and nothing consumes their accessors; removing them is pending follow-on work (kept for now because `RunContextManager` shares the `Conversation` bundle and *is* used, being registry-backed). See blueprint §19.

Direction of travel, decided but not yet executed: strip the **power-user surface** (self-rewriting, arbitrary shell, dev-agent suite, ticket system) and move to a **binary-first** layout — the app is built once and run from a compiled binary, not executed from its own source tree.

## Workspace layout

The application core is the `skald-core` crate; the binaries are **shells** around it.

| Crate | Role |
| ---- | ---- |
| `crates/skald-core/` | Storage, identity, crypto, LLM stack, tools, MCP, sessions. Knows nothing about what runs it: no HTTP server and **no concrete plugin crate** — `PluginManager` only ever sees `Arc<dyn Plugin>` from `core-api` |
| `skald` (root, `src/`) | The server shell: `main.rs`, the Axum `frontend/`, `config.rs`. Constructs the plugin list and hands it to `Skald::new`. Runs headless as a background daemon under the `run.sh` supervisor |
| `crates/skald-setup/` | Guided first-run setup — a terminal shell over `skald-core`. Creates the first admin and seeds the instance through the **shared seam `skald_core::setup::initialize_instance`** (apply the chosen seed profile → `register_user(admin)` → set default locale) — the *same* function the web setup calls, so the two shells can't drift. Asks profile, interface language, whether to encrypt — default yes — and password. A separate binary so the server never links TTY-prompt deps, and so a future GUI installer is a third shell over the same seam. `run.sh` runs it before the server loop; it prompts only when `users` is empty **and** stdin is a terminal, otherwise a no-op. `--check` reports readiness by exit code (0 done, 1 needed) |
| `crates/core-api/` | The contracts both sides share: `Plugin`, `Tool`, event buses, provider types |

Two rules keep the boundary real, and both are enforced by the compiler:

- **The core never names a plugin.** A plugin contributes tools through `Plugin::tools(self: Arc<Self>)` — the sibling of `http_router()` — so nothing in the core has to downcast to a concrete type. Naming one would drag every plugin in the tree into the core, including a C build via `plugin-transcribe-whisper-local`.
- **The core never learns about the process shell.** The `restart` tool defaults to the supervisor protocol (`exit(-1)`); a shell with different needs (e.g. one with no supervisor) can install its own `tools::restart::set_restart_handler` at startup. The default server shell installs none and relies on `run.sh`. The seam stays even though nothing installs a handler today.

**Plugin visibility & per-user config.** The admin surface is split in two: `#plugin-catalog` (`plugin-catalog.js`) is a status board — one card per plugin with an enable toggle + health dot + a Configure button — and `#plugin-detail?id=<id>` (`plugin-detail.js`) holds the instance-config form + per-user access checklist for one plugin (the plugin counterpart of `connector-detail.js`). The user-facing half is `#plugins` (`plugins-page.js`): granted plugins + their per-user config forms. Enable/disable + instance config + access grants are gated by the `plugin.manage` capability (admin-only by construction). Visibility is **opt-in**: a row in `plugin_access(plugin_id, user_id)` grants a user sight of an enabled plugin (`plugin_id` is bare TEXT, never a FK — a `plugins` row exists only after the first toggle). A plugin with a non-empty `Plugin::user_config_schema()` exposes per-user settings, stored in `plugin_user_configs` (**admin-readable system.db — never secrets**) and applied through the `Plugin::update_user_config` hook, whose default just stores the blob via the `PluginUserConfigApi` on `PluginContext.user_config`. Telegram is the reference impl: the user pastes the bot's pairing code in their Plugins page, the override turns it into a `chat_id → user_id` binding (same write path as the `telegram_pairing` tool) and stores a `{linked, chat_id}` status blob for the UI. Endpoints: admin `GET/PUT /api/plugins[/{id}]` + `GET/PUT /api/plugins/{id}/access`; user `GET /api/plugins/mine` + `PUT /api/plugins/{id}/my-config`.

**Plugin HTTP routes & web pages.** Every plugin's `http_router()` mounts at boot under `/api/plugin/<id>/` — **enabled or not**: two shared gates wrap each router (`require_auth`, then `guard::plugin_enabled_gate`, which re-checks the DB flag per request and answers 404 while disabled), so enable/disable serves/stops routes immediately with no restart, and plugin responses carry `Cache-Control: no-cache`. The router contract: cheap and safe to build pre-start, handlers tolerant of the not-running state (resolve runtime state per request through a shared cell, as mobile-connector does). A plugin may also contribute **frontend pages** via `Plugin::web_pages()` (`PluginPage { page_id, title, icon, entry, admin_only, priority }`): `GET /api/plugins/pages` returns the caller's visible pages (admin: all; others: non-`admin_only` pages of granted, enabled plugins) with `entry_url` resolved, and the sidebar renders them as menu entries routed `#plugin/<plugin_id>/<page_id>`. A single `<plugin-page-host>` (`web/components/plugin-page-host.js`) dynamic-imports the fragment ES module the plugin serves from its own router, registers its default-exported HTMLElement class, and mounts it with the `plugin-id` attribute — the fragment talks to its backend only through `/api/plugin/<id>/…` and runs with full session privileges (plugins are trusted: they ship in the binary). The frontend knows nothing about plugin page contents or behavior.

`skald_core::boot` emits curated startup lines on the `boot` tracing target; each shell decides how to render them (`src/boot_format.rs` here). The core says what happened, never how it looks.

## Key modules

| Path | Role |
| ---- | ---- |
| `src/main.rs` | Thin entry point: tracing → `Skald::new` → `WebFrontend::start` → shutdown. Builds a tokio runtime and blocks on `async_main`, which runs the backend until a SIGINT/SIGTERM. Exposes `run_backend()` / `shutdown_backend()` |
| `crates/skald-core/src/skald/` | `Skald` — headless application core. `mod.rs` (struct + staged `new()` / `shutdown()`), `runtime.rs` (cross-cutting `Runtime` context), `bundles.rs` (8 domain bundles + `build()`), `wiring.rs` (`wire()` + `spawn_background()`), `supervisor.rs` (`TaskSupervisor`), `accessors.rs` (per-manager accessor facade — the API surface the frontend uses) |
| `crates/skald-core/src/session/handler/` | Core LLM loop — `mod.rs`, `llm_loop.rs` (`run_agent_turn`), `agent_dispatch.rs`, `dispatcher.rs`, `approval.rs`, `resume.rs`, `messages.rs`, `config.rs`, `interface_tools.rs`, `media.rs` (multimodal attachments — see below) |
| `crates/skald-core/src/session/manager.rs` | Creates/retrieves `ChatSessionHandler` per session |
| `crates/skald-core/src/chat_hub/` | `ChatHub`: broadcast events to all connected WS clients |
| `crates/skald-core/src/chat_event_bus.rs` | Global async bus for cross-session events |
| `crates/skald-core/src/agents.rs` | Discovers agents from `agents/*/`, loads meta + system prompt |
| `crates/skald-core/src/tools/` | Built-in tools: `exec` (**runs inside the caller's per-user Docker container** via `docker exec`, as the non-root host uid — `sudo` for system installs — with a robust /stop that reaps the command's process-group; see `container/`), `restart`, `list_agents`, `fs/*` (route `user-memory/`/`shared-memory/` to `memory_docs`, and every other **physical** path through `ctx.fs` to the caller's per-user host workspace — see DB tables + container), `notify`, `ast_outline`, `image_generate`, MCP tools, plugin tools, cron tools |
| `crates/skald-core/src/container/` | `ContainerManager` (§6): per-user Docker containers (the execution sandbox). Docker is a **hard requirement** — `check_docker()` fails `Skald::new` (→ shell exits) if the daemon is unreachable. Builds our own `skald-runtime` image (python+node+**sudo**; tag is **versioned** `skald-runtime:v2` so a `Dockerfile` change forces a rebuild) once from the embedded `Dockerfile`, then `reconcile_all()` at boot ensures one running container `skald-{userid}` per active user. Each container runs as the **host `uid:gid`** (`--user`, §6 UID coherence) with `--init` (tini reaps zombies); `ensure()` **self-heals** a container whose `--user` is stale (e.g. an old root one) by recreating it, and injects a passwd/shadow entry post-create so `sudo` (NOPASSWD, in the image) resolves the arbitrary uid. `build_user_fs()` assembles a user's `UserFs` (home `{WD}/homes/{userid}` → `/root`, plus each `shared/{name}` they belong to). Shells the `docker` CLI (no client crate) |
| `crates/skald-core/src/tool_catalog.rs` | `ToolCatalog`: unified tool listing façade (wraps ToolRegistry + McpManager) |
| `crates/skald-core/src/events.rs` | `ServerEvent` enum streamed over WebSocket to the frontend |
| `crates/skald-core/src/db/` | sqlx SQLite — see below |
| `crates/skald-core/src/users/` | `UserManager` (§11): user directory CRUD on `system.db`, credential check, and the map `userid → SqlitePool` of **unlocked** databases. The pool *is* the unlock token — its connect options carry the DEK as SQLCipher's raw key, so an open pool means the key is in RAM (§9) and dropping it re-locks. Knows nothing about cookies: whatever maps an HTTP session to a user id sits above it |
| `crates/skald-core/src/crypto/` | Envelope encryption (§4/§5.1). A random 256-bit DEK encrypts `{userid}.db`; `users.database_password` holds it sealed with AES-256-GCM under `Argon2id(password, salt)`. **The AEAD tag is the password verifier** — one derivation both authenticates and yields the key, and no second hash sits in the admin-readable DB. Cleartext users store the Argon2id output directly, compared constant-time. Argon2 runs in `spawn_blocking` behind a 2-permit semaphore (256 MiB per derivation) |
| `src/config.rs` | Loads `config.yml`; LLM clients, strength/use_cases, data root. All relative paths (db, logs, data, …) resolve against the launch cwd |
| `crates/skald-core/src/mcp/` | MCP runtimes + the `McpProvider` seam (§7): the shared host **global** runtime and the per-user **container** runtimes, unioned per session as `UserMcpView`. See the MCP connectors section |
| `crates/skald-core/src/plugin/` | Plugin system: discovery, enable/disable, tool registration, per-user access grants + per-user config |
| `crates/skald-core/src/cron/` | Scheduled job runner |
| `crates/skald-core/src/compactor.rs` | Context compaction (summarises history when token budget exceeded) |
| `crates/skald-core/src/approval/` | Approval rules engine |
| `crates/skald-core/src/clarification/` | `ClarificationManager`: background-session question/answer |
| `crates/skald-core/src/elicitation/` | `ElicitationManager` + bridge: MCP server-initiated input (`elicitation/create`), surfaced in the Inbox; secrets never logged/persisted |
| `crates/skald-core/src/inbox.rs` | `Inbox`: unified façade for pending approvals + clarifications + elicitations (wraps ApprovalManager, ClarificationManager, ElicitationManager) |
| `crates/skald-core/src/llm/` | LLM client abstraction (OpenAI-compat, Anthropic, Ollama…). OpenAI-compatible provider *types* are runtime data, not code: `providers/declared.rs` loads `providers.yaml` at boot (see Config); only non-OpenAI-compatible or bespoke providers (anthropic, ollama, openai, openrouter) stay native |
| `crates/skald-core/src/transcribe/` | Transcription providers |
| `crates/skald-core/src/image_generate/` | Image generation providers |
| `crates/skald-core/src/memory/` | Agent memory tools |
| `src/frontend/mod.rs` | `WebFrontend`: wires router_factory, starts plugins, runs Axum |
| `src/frontend/server.rs` | Axum router, static file serving |
| `src/frontend/api/` | HTTP + WebSocket handlers — `State<Arc<Skald>>` |
| `web/components/` | Lit web components (see below) |

## DB tables (sqlx SQLite)

`database/system.db` — the path is a constant (`core::db::SYSTEM_DB_PATH`), **not** configurable. `init_system_pool` creates the directory; SQLite only creates the file. Per-user files are `database/{userid}.db`, created by `UserManager::register_user` and encrypted with SQLCipher.

The schema is split into two buckets (§5.1), and the split is the point:

- **`create_registry_tables`** — instance-wide, readable without any user key: `users`, `roles`, `llm_providers`, `llm_models`, `transcribe_models`, `tts_models`, `image_generate_models`, `plugins`, `plugin_access` + `plugin_user_configs`, `approval_rules`, `tool_permission_groups`, `config`, `known_tools`, `llm_requests`, `mcp_catalog`, `mcp_global_servers` + `mcp_global_access`, `oauth_providers`, `role_capabilities`, `shared_folders` + `shared_folder_members`. The MCP tables back the Connectors model (§7/§14/§15 — see its own section); `oauth_providers` (accessor `db/oauth_providers.rs`) holds one row per identity provider (Google…) — endpoints + `client_id`/`client_secret` + `redirect_uri`, admin-owned household secrets (§4/§15b), never a per-user token. The last two (accessor `db/shared_folders.rs`) are the **membership** of the on-disk shared folders (§6): a junction table so a member can be read-only (`can_write`) and so the container mount topology + the `shared/{X}` fs routing both query it. FK `shared_folder_members.user_id → users(id)` is registry→registry (same file), which is allowed — unlike an owner→registry key.
- **`create_owner_tables`** — one owner's content, **identical schema in every file that has it**: `chat_sessions`, `chat_sessions_stack`, `chat_history`, `chat_llm_tools`, `chat_summaries`, `session_scratchpad`, `session_mcp_grants`, `stack_mcp_grants`, `scheduled_jobs`, `job_runs`, `mcp_user_servers`, `mcp_events`, `sources`, `secrets`, `projects`, `project_tickets`, `llm_request_payloads`, `memory_docs` (+ FTS5 `memory_docs_fts`). `mcp_user_servers` (a user's activated per-user connectors) carries `catalog_name` as a **bare `TEXT` snapshot** of `mcp_catalog.name`, never a FK — an owner→registry key would fail every INSERT; for an OAuth connector it also snapshots `oauth_provider` + `deliver_json`, and its `api_key` column holds the refresh token (in the SQLCipher-encrypted file, so no column crypto). Because `memory_docs` is an owner table, one definition backs **private** memory in each `{userid}.db` and **shared** memory in `system.db` (the household owner) — see the memory namespace note below.

Schema is greenfield (no migrations, §0), but a purely **additive** column lands on an existing DB in place: `db::ensure_column` runs `ALTER TABLE … ADD COLUMN` and swallows the "duplicate column" error, a no-op on a fresh DB where the `CREATE TABLE` already has the column. Used for the OAuth columns on `mcp_catalog` / `mcp_user_servers` so a dev box need not be wiped for an additive change (a full recreate is still valid).

**No foreign key in the owner bucket may point at a registry table.** SQLite cannot enforce a key across files, not even through `ATTACH`, and sqlx turns on `PRAGMA foreign_keys`: the `CREATE TABLE` succeeds and every `INSERT` fails. `db::tests::owner_tables_stand_alone_with_foreign_keys_on` enforces this by running the owner schema against a database holding nothing else, then inserting a row into each table. Two keys crossed and were fixed: `chat_history.model_db_id` (dropped — write-only, and `llm_requests.model_name` already records the model) and `project_tickets.job_id` (fixed by moving `projects`/`project_tickets` into the owner bucket).

**Memory namespace (blueprint §5).** `memory_docs` (accessor `db/memory_docs.rs` — `get`/`upsert`/`list`/`search`(FTS)/`delete`) backs a virtual note store surfaced through the fs-tools, **not** the disk. Two sibling roots (not the blueprint's nested `memory/{userid}` + `memory/shared`): `user-memory/…` routes to the caller's own pool (`ToolContext::pool`), `shared-memory/…` to the system pool (a singleton captured in `fs::register_all`). `tools/fs/classify_memory()` decides on the raw first path component (a `..` in the tail clamps inside the store, never escapes to disk); `read_file`/`write_file`/`list_files`/`edit_file`/`insert_at_line`/`replace_lines`/`search_file` override `run_with` to route memory paths (each extracting a pure transform shared with its on-disk `execute`) and leave every other path on disk. Approval (seeded in `seed_fs_path_rules`): `user-memory/*` is `@fs_any allow` (private, frictionless); `shared-memory/*` is `@fs_read allow` + `@fs_write require` — reads free, **writes need approval** so the agent can't silently push one person's data into shared memory. `grep_files` stays disk-only (regex-across-tree ≠ FTS); ranked full-text recall over notes is a separate tool, `memory_search` (`tools/fs/memory_search.rs`), over the `memory_docs` FTS index — allowed by a path-less rule (it takes `query`, not `path`).

**Memory injection into the prompt**: `MessageBuilder::load_inject_memory` routes each `meta.inject_memory` entry — `user-memory/…` → owner pool, `shared-memory/…` → the shared (`system.db`) pool, both via `memory_docs::get`; anything else (`data/…`, `$WD/…`) is a disk read. The shared pool is threaded `ChatSessionManager` → handler → `MessageBuilder`. `main` and `project-coordinator` inject `user-memory/index.md` + `shared-memory/index.md`.

**Prompt substitutions**: an `AGENT.md` may carry `<!-- KEY -->` placeholders; `agents::resolve_includes` turns each into a `__KEY__` sentinel, replaced at request time. Two are **builder-side** — `MessageBuilder` resolves them itself from the session owner (`user_id`) + registry (`shared_pool`), so every source (WS, mobile, cron, sub-agents) gets them with no caller plumbing: `__SHARED_FOLDERS__` (the user's shared-folders table) and `__USER_PROFILE__` (the owner's directory profile: `Name`, `Date of birth` with age computed at build time, `Sex`, `Preferred language`, admin `Notes` — unset values render as explicit `unknown` / `not specified`, the `Notes` line is omitted when empty). Any other key comes from the per-call `SendMessageOptions::system_substitutions` map.

`system.db` still gets **both** bucket functions — but no longer because the migration is unstarted. It gets the owner schema because it *is* the owner of **shared** memory (`memory_docs`) plus, for now, the globally-scoped `secrets` and the `mcp_events` lifecycle log (`SecretsStore` and the global `McpManager` are built on the system pool and shared by reference into every `UserContext`; the global runtime's *config* now lives in the registry table `mcp_global_servers`, and per-user connector config in each user's owner `mcp_user_servers`). Every *other* owner table is created there but never written to anymore — the global owner-bound managers that would write them (chat/jobs/etc.) are inert (see "Current state"). Fully dropping `create_owner_tables` from `system.db` is blocked on the §4 scope decision for secrets (plus the residual global `mcp_events` log), not on call-site migration.

`users` (`crates/skald-core/src/db/users.rs`) holds the directory plus auth material. It lives in the system DB, which the box owner can read, so it must never store anything that derives a user's key. `Credentials` is an enum mirroring the table's `CHECK`: an encrypted user carries a **wrapped DEK** (whose AEAD tag *is* the password verifier — hence no `password_hash`); a cleartext user carries an ordinary verifier, or none. `User` is deliberately not `Serialize` and its `Debug` redacts key material — use `User::summary()` for anything leaving the process. `role_id` references `roles(id)` (the `roles` table is now seeded before `users` in `create_registry_tables`). A nullable `locale` column (additive via `ensure_column`) holds the per-user UI language override; role-driven conventions live in the free-form `roles.attrs` JSON — never new columns per attribute — parsed at a **single point** by the typed `db::roles::RoleAttrs` (`ui_mode`, `permission_groups`): `ui_mode` (see the frontend section) plus the role's **security-group set** (`roles.permission_group` = the default group, `attrs.permission_groups` = additional allowed groups; `Role::effective_groups()` = the union, `roles::role_allows_group()` gates it with `admin` short-circuiting to all). See the security-group picker in the frontend section. The admin-managed **directory profile** lives in three more additive columns — `birthdate` (ISO `YYYY-MM-DD`), `sex` (free text), `notes` (admin-authored) — edited only from the Users admin page (`set_directory_fields`; validation — real non-future date, length caps — lives in the `users_mgmt` API, not the db layer) and rendered into agent prompts by the `__USER_PROFILE__` substitution (see above). They are directory metadata written *by* the admin *about* the user, so the registry is their honest home under the §2 threat model.

## Filesystem & containers (blueprint §6)

Each user has one **permanent Docker container** (`skald-{userid}`, our own `skald-runtime` image with python+node), created on user creation and started at boot (`ContainerManager`, `crates/skald-core/src/container/`). Docker is **required**: a missing daemon fails `Skald::new` and the process exits. The container runs as the **host `uid:gid`** (not root) so files created in-container and by the host-side fs-tools share ownership on the bind mounts (matters on native Linux; masked on macOS Docker Desktop). Because that user isn't root, the image ships passwordless `sudo` (a passwd/shadow entry is injected at create) so an agent can still `sudo apt-get install …`; `--init` runs tini as pid 1 to reap zombies.

The agent sees **one namespace**, routed on the first path component. The choke point is `UserFs` (`core-api/src/user_fs.rs`, a pure value type carried in `ToolContext.fs`), plus `resolve_host_path()` in `tools/fs/mod.rs`:

| Agent path | Backing | Routed by |
| ---- | ---- | ---- |
| `user-memory/…` | SQLite `ctx.pool` (`{userid}.db`) | `classify_memory` → `memory_docs` |
| `shared-memory/…` | SQLite `system.db` | `classify_memory` → `memory_docs` |
| `shared/{X}/…` | host `{WD}/shared/{X}` (if a member) | `UserFs::host_base_and_tail` |
| `~/…`, relative | host `{WD}/homes/{userid}` | `UserFs::host_base_and_tail` |

Two views, **one storage**: the fs-tools run **host-side** in the Skald process on `{WD}/homes/{userid}` + `{WD}/shared/{X}`; `execute_cmd` runs **inside the container** (`docker exec -w <container-path> skald-{userid} sh -c …`, via `ExecuteCmd::run_with`) on the same paths bind-mounted (`homes/{userid}`→`/root`, `shared/{X}`→`/root/shared/{X}`, read-only when `can_write=0`). A file written in the container appears to the host fs-tools and vice versa.

**Containment** (`resolve_host_path`): every physical fs-tool op canonicalizes the resolved path (following symlinks) and prefix-checks it against its mount base, **fail-closed**. Since the same tree is writable from inside the container, a symlink planted there that points outside the home/shared root is caught here — the host-side tool never escapes the user's workspace. `grep_files` stays disk-only (regex ≠ FTS; memory → `memory_search`) but resolves its root the same way. `execute_cmd`'s `workdir` is an agent path mapped to its container path via `UserFs::to_container`.

The threading: `UserContext.fs` (built by `container::build_user_fs` at login, snapshotting shared memberships) → `ChatSessionManager` → `ChatSessionHandler.fs` → `ToolContext.fs`. **Admin CRUD is wired** (`src/frontend/api/shared_folders.rs` — `GET/POST /api/shared-folders`, `PATCH/DELETE /api/shared-folders/{id}`, `POST`/`DELETE .../members[/{user_id}]`; UI `shared-folders.js`): a create/describe/delete + per-member `can_write` surface, and each mutation calls a best-effort `remount(user)` that rebuilds the affected user's fs + container mounts **in place** — so a membership change lands without a re-login (blueprint §6's "admin CRUD" + "membership refresh without re-login" TODOs, now closed; it still settles at next login/boot if the live remount fails). `execute_cmd` /stop is robust: the command runs under `setsid -w` in its own process-group (leader pid recorded in a container pidfile), and a `KillReaper` drop-guard reaps that group on /stop **or** timeout via a detached `docker exec` that walks `/proc` and kills members by **positive pid** (the container's dash mishandles `kill -<pgid>`); the pidfile is passed positionally (`$1`), and the container's `--init` (tini) reaps the killed processes so no zombies accumulate. **Per-user MCP connectors now run inside this container** (§7) — the container infra enabled it; see the MCP connectors section.

## MCP connectors (blueprint §7/§14/§15)

MCP servers are surfaced to users as **"Connectors"** (UI naming; `mcp`/schema stays neutral, §0.1). The old single owner table `mcp_servers`, the agent-facing `register_mcp`/`delete_mcp` tools, and the `mcp` kinds of `list_items`/`toggle_item` are **gone**. Connectors are now admin-curated and user-activated through the Connectors UI/API — never written by the agent, which closes the §14 RCE vector (prompt-injection → agent writes+registers a local script → arbitrary code on the box).

**Two runtimes, one view (§7).** A session's MCP tools are the **union** of:

- **Global runtime** — shared, stateless connectors (web-search, Tavily…) that run on the **host**, connected at boot from `mcp_global_servers` by `McpManager::initialize`. Filtered per user by `mcp_global_access`.
- **Per-user runtime** — the connectors a user has activated, run **inside their container**, started at first login from that user's owner `mcp_user_servers` and living until restart (§9; the `docker exec -i` children die via `kill_on_drop` when the `UserContext` drops).

`McpProvider` (`mcp/provider.rs`) is the trait the session code talks to, so `all_tool_defs` / `render_mcp_list` / `ActivateTools` never learn which runtime owns a server. `McpManager` implements it directly (used for the inert ownerless bundle, §19); `UserMcpView` implements it as `global ∪ user`, where `accessible_global` is a snapshot of `mcp_global_access` captured when the `UserContext` is built (like fs membership). Both runtimes share `McpManager::connect_all(specs, boot)`; `McpServerSpec` + `global_row_spec`/`user_row_spec` turn a DB row into a connectable spec (a per-user `local_script` spec targets the user's container).

**Authorization is a capability on the role, not `if role==admin`** (§0.1/§14 — `db/role_capabilities.rs`): `mcp.register_remote` + `mcp.register_local_from_catalog` are self-service (seeded on every new role by `roles::create` via `seed_defaults`); `mcp.register_local_script` + `mcp.manage_catalog` are admin-only. `admin` holds every capability by construction (short-circuit in `has()`). API handlers gate through `require_cap`.

**Tables** (see DB section) — registry: `mcp_catalog` (admin-vetted templates; holds only the *schema* of what an activation must supply, never live creds — plus, for OAuth, `oauth_provider` + `oauth_scopes_json` + `deliver_json`), `mcp_global_servers` + `mcp_global_access`, `oauth_providers` (per-provider client creds), `role_capabilities`. Owner: `mcp_user_servers` (per-user activations; `api_key` encrypted at rest — the refresh token for an OAuth one — `catalog_name`/`oauth_provider`/`deliver_json` bare `TEXT` snapshots).

**Endpoints** (`src/frontend/api/mcp.rs`, mounted in `api/mod.rs`) — admin: `/mcp/catalog` (GET/POST/DELETE), `/mcp/global` (list/enable/delete + `/{id}/access` GET/PUT), `/mcp/providers` (GET/POST + DELETE `/{name}` — OAuth provider creds, secret never returned to the browser). User: `/mcp/available`, `/mcp/activate`, `/mcp/activated` (+ DELETE `/{id}` to deactivate), `/mcp/oauth/start` + `/mcp/oauth/complete` (the §15 OAuth login), `/mcp/login/status` + `/mcp/login/reset` (the §15 QR/device login — see below). `connectors.js` (`<connectors-page>`) renders the user view (activate/deactivate + granted globals) always, plus the admin view (catalog + global + per-server access + a **Sign-in providers** modal) when `role_id === 'admin'`; `connector-detail.js` (`<connector-detail-page>`) is a connector's own page and hosts both the OAuth login panel and the QR login panel.

**Dependency reconciler (`mcp::install::ensure_installed`).** Copying a local-script connector's files into a container never installed its deps. `ensure_installed` closes that: a **content-hash reconciler** keyed on the connector's *source* files (not a version string) that, when the hash changed, re-copies the files and installs deps inside the container — `npm ci --omit=dev` (node, from `package.json`) and/or `pip install --target .pydeps` (python, from `requirements.txt`, put on the server's `PYTHONPATH` by `user_row_spec`). Runs at activation **and** on every per-user startup path (`UserContext` build, remount) via `mcp::prepare_local_connector`, so a fresh container installs from scratch, an updated connector re-installs, and an unchanged one is a hash-match no-op. Deps are therefore **never vendored** — connectors ship `package.json`/`requirements.txt`, not `node_modules/`. Authoring contract for connectors lives in `scripts/CONNECTOR_MANIFEST_GUIDE.md`.

**Connector versioning.** `mcp_catalog` carries `version` (INTEGER — the update-comparison key), `version_string` (semver, display) and `version_release_date` (ISO, display), snapshotted from the feed on install. The marketplace list computes `update_available` = feed `version` > installed `version` (strict) and surfaces it as an "Update" button (`marketplace.js`). The integer is the UI signal; the actual re-install trigger is the reconciler's content-hash.

### OAuth per-user connectors (blueprint §15 — copy-paste flow)

OAuth2 authorization-code + PKCE is wired for per-user connectors (Gmail is the first). The consent is a **human copy-paste**, not a headless action: no callback route into the (NAT'd, hostname-less) box, and no client secret on the public feed.

- **Providers, not per-connector URLs.** The client is per-**provider** (one Google app covers Gmail/Calendar/Drive): `oauth_providers` holds `auth_url`/`token_url`/`client_id`/`client_secret`/`redirect_uri`/`extra_params`, admin-entered via the Sign-in-providers modal (Google preset fills all but the two secrets; `redirect_uri` = the static `oauth/show.html` page, `extra_params` = `access_type=offline`+`prompt=consent` so Google returns a refresh token). The manifest only names `auth.provider` + `auth.scopes` + `auth.deliver` — never URLs or secrets (feed is remote data, §14).
- **Flow** (`mcp/oauth.rs`): `activate` on an OAuth catalog entry persists a **pending** `mcp_user_servers` row (files installed, command wired, no token) and returns `needs_oauth` — it does **not** start the server. `/mcp/oauth/start` builds the consent URL (PKCE S256 + opaque `state`) and stashes the verifier in a RAM-only, TTL'd flow store keyed by `state`; the user approves in a browser, the provider lands the code on `oauth/show.html`, they paste it back. `/mcp/oauth/complete` exchanges code+verifier for a refresh token (`client_secret` sent server-side), stores it in the row's `api_key`, flips to `ready`, and starts the server. PKCE makes an intercepted code worthless; a restart drops in-flight flows (mirrors the RAM-only session model).
- **Credential delivery = env, nothing on disk.** The manifest's `deliver` (`{as,format,env}`, parsed as `mcp::DeliverSpec`) says how the token reaches the server. `user_row_spec_resolved` assembles the credential (`google_authorized_user` JSON = client creds from the provider + refresh token) and injects it as an env var (`GMAIL_CREDS_JSON`) on the `docker exec` — never a file, coherent with §2 (the tempted admin doesn't read `/proc`). The server reads it via `Credentials.from_authorized_user_info`. Ran both at OAuth-complete and at login-time per-user startup.
- **Google needs a Web-application client**: a Desktop client rejects an `https://` redirect (loopback only), so the `oauth/show.html` redirect must be registered on a **Web app** OAuth client, and exact-match under Authorized redirect URIs — `redirect_uri_mismatch` otherwise.

### QR / interactive device login (blueprint §15 — polling flow)

For a per-user connector whose credential is produced by **pairing** (`auth.type: "qr"`; WhatsApp is the first, on Baileys — the slim `skald-runtime` image has no Chromium, so a browser-based client is out), there is no code to paste and the server must **run** to produce the QR. The seam is a generic tool contract, reusable for future device kinds (SSH…):

- **`login_status` tool contract.** A connector needing an interactive login exposes one tool, `login_status`, returning JSON `{state, qr?, message}` (state: `connecting|need_scan|ready|logged_out`; `qr` is a data-URL PNG only while `need_scan`). Skald calls it **directly, never the agent**.
- **Flow.** `activate` on a `qr` entry inserts a **pending** `mcp_user_servers` row and **starts** the server (unlike OAuth, which defers), returning `needs_login`/`login_kind:"qr"`. `/mcp/login/status` ensures the server is running (restarts a pending one), calls `login_status`, and returns its state; on `ready` it flips the row's `auth_state` so `all_startable` picks it up next login. `/mcp/login/reset` calls the connector's `logout` tool to re-arm (link a different device). The `connector-detail.js` QR panel polls `login/status` and renders the QR.
- **Credential = on-disk session, not a token.** The connector persists its session inside its own dir (e.g. `./auth/`), under the bind-mounted home so it survives a container recreate — the honest §4 gap (admin-root-readable), not `memory_docs`.
- **Node 18 gotcha**: the container ships Node 18; Baileys uses the Web Crypto global, so the server must `globalThis.crypto ??= require('crypto').webcrypto` or it dies pre-QR with "crypto is not defined".

**Deferred:** SSH and other §15 device kinds (would reuse the `login_status` contract), `deliver.as=file`, and non-Google OAuth providers are unimplemented paths that error clearly rather than half-work. No boot seed of catalog presets; the admin populates the catalog from the Marketplace.

## Multimodal attachments

Uploads (`POST /api/{source}/uploads`) are saved per-user under `data/uploads/{userid}/{session_id}/` (older rows may still reference the pre-namespacing `data/uploads/{session_id}/` layout — both stay readable), streamed to disk with a 256 MiB cap, with the sniffed magic-byte MIME preferred over the client claim; `/data/*` is served behind the same session-cookie gate as `/api`. Attachment metadata travels as structured JSON in `chat_history.metadata` — never as persisted text.

At context-build time (`MessageBuilder`), attachments of the **current turn** (the user/agent rows following the last completed assistant reply, including across in-flight tool rounds) are partitioned by `session/handler/media.rs`: when the resolved model's `LlmEntry.capabilities` include the modality (`vision` → `image_url` parts, `video` → `video_url` parts), the file is inlined as a base64 data-URL content part — but only if it canonicalizes under `data/uploads/`, its sniffed MIME is in the allowlist, and it fits the budgets (4 files / 10 MiB image / 32 MiB video / 48 MiB total per turn). Everything else — older turns, other kinds, any failed check — keeps the textual `[SYSTEM INFO]` path block, so a non-vision model produces a byte-identical payload to before. `OpenAiClient` forwards parts verbatim; `AnthropicClient` translates `image_url` data URLs to `image` blocks (video unsupported; Anthropic models get `vision` by editing the model row's capabilities — no catalog refresh writes them). On LLM fallback mid-round, messages are rebuilt with the replacement model's capabilities.

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

**Tool visibility in the Security-groups UI** (`GET /api/approval/tools`): tools injected outside the `ToolRegistry` (interface/plugin/provider tools) would otherwise be un-configurable. `ToolCatalog::list_all()` covers registry tools + a static `synthetic_tools()` list of core interface tools; everything else is captured by `crates/skald-core/src/tool_discovery.rs` (`ToolDiscovery`), which taps `all_tool_defs()` in `llm_loop.rs` each round and upserts every offered tool into the `known_tools` table (in-memory seen-set guard → background DB write). `list_tools` merges `known_tools` (deduped, `category: "dynamic"`) so any tool offered at least once becomes gate-able. Drift-proof by construction; core never hardcodes plugin tool names.

## Restart

`restart` **no longer rebuilds anything** — it does not compile.

No restart handler is installed, so `restart` calls `libc::_exit(-1)` (= exit code 255); `run.sh` re-executes the same binary *by path*. (The `set_restart_handler` seam stays for a hypothetical shell without a supervisor, but nothing installs a handler today.)

Use it to pick up `config.yml` / `providers.yaml` / database changes, which are only read at startup. To load new **code**: `./build.sh`, then restart — the supervisor picks up the new binary on the next loop, since `build.sh` installs it with an atomic rename.

> `run.bat` is still stale (`cargo run`) and must be fixed.

## Build & run

```sh
./build.sh      # release build → bin/skald and bin/skald-setup (atomic install)
./build.sh -d   # debug profile; extra args are forwarded to the server build
./run.sh        # first-run setup, then the supervisor loop — never compiles
```

`build.sh` builds and installs **both** binaries; any forwarded args go to the server only.

`run.sh` resolves the server binary as `$SKALD_BIN` → `bin/skald` → `target/release/skald`, and warns when sources are newer than it. Before the loop it runs `skald-setup` (found next to the server, or `$SKALD_SETUP_BIN`); a non-zero exit there — a failed or cancelled wizard — stops `run.sh` before the server starts. Server exit `0` stops the loop, `255` re-executes, anything else propagates.

> In a **debug** build, Argon2id at 256 MiB is unoptimised and takes far longer than the ~1s of a release build — `skald-setup -d` will feel stuck at the password step. Use the release binary for anything interactive.

Tracing filter: `RUST_LOG=skald=debug,info`

## Adding an agent

Create `agents/<id>/meta.json` and `agents/<id>/AGENT.md`. The agent is discovered at runtime (no restart needed for prompt edits). Optionally set `"client": "<name>"` in meta.json to pin a specific LLM.

## Documentation

The `docs/` directory is **ignored** for now — do not read it, reference it, or update it. It is slated for removal.

## Config

Copy `default.config.yaml` → `config.yml`. Never commit `config.yml` (contains API keys).

`providers.yaml` (repo root, cwd-relative like `config.yml`) declares the **OpenAI-compatible LLM provider types** — endpoints, UI metadata, per-model JSON field mapping, id-glob enrichment rules, reasoning knobs. Loaded at boot by `llm::providers::declared`; edit + `restart`, no rebuild. An invalid entry is logged and skipped, never fatal; an `id` colliding with a native provider is skipped. Adding a new OpenAI-compatible provider is a YAML edit, not a Rust file. The shipped file is validated by a unit test (`declared::tests::shipped_providers_yaml_is_valid`).

## Python environment

All Python scripts (MCP servers, setup scripts) use a local virtualenv at `.venv/` in the project root.

`run.sh` creates it automatically on first launch (using `uv` if available, otherwise `python3 -m venv`) and installs `requirements.txt`. It then prepends `.venv/bin` to `PATH` before starting the app, so every child process — MCP server launches, `execute_cmd` shell calls — resolves `python3` to the venv automatically. No manual activation needed. **Python is optional**: if neither `uv` nor `python3` is found, the app starts normally and only Python-based MCP servers will be unavailable.

To add a Python dependency: add it to `requirements.txt`. It will be installed on the next `./run.sh` invocation if `.venv` does not yet exist — or run `uv pip install -r requirements.txt` manually.

## Frontend components (`web/components/`)

All extend `LightElement` from `web/lib/base.js` (Lit). `ChatSession` (`web/lib/chat-session.js`) is the shared base for WS-connected chat UIs.

**The chat is the home page.** `<app-copilot>` is a single persistent element with two layout modes driven by the route (`llm-page-change`): `mode="full"` on the home route (it fills the workspace — the conversation IS the landing page, with a welcome hero + prompt suggestions as its empty state) and `mode="dock"` on every other route (the classic resizable side panel). Same element ⇒ WS, tabs, scroll and drafts survive navigation; you watch files/projects update live while the conversation keeps going. Collapse only applies to the dock. The old dashboard content (hero, LLM stats charts, pending inbox, quick guide) lives on as the separate `#dashboard` page; the debug toggle moved to the Settings page.

**Theme** (`web/css/variables.css`): warm "paper" palette (terracotta accent, light by default, warm-charcoal dark), generous radius (`--radius-sm/md/lg`), 16px-base chat type, WCAG-fixed contrasts, global `:focus-visible` ring and `prefers-reduced-motion` support. Everything consumes CSS variables — never hardcode a hex in a component stylesheet.

**i18n** (`web/lib/i18n.js` + `web/i18n/{en,it,fr}.js`): `t(key)` helper, `I18nMixin` re-renders on `locale-changed`. Resolution order: user preference (`users.locale`, editable on the profile page) → instance default (registry config key `ui_locale`, editable by the admin in Settings — declared in `skald_core::i18n::config_set`) → English. **Server-side, never re-implement that chain**: `skald_core::i18n::resolve_locale(pool, user_locale)` is the one function (with `default_locale(pool)` and `language_name(locale)` for prompt rendering); they read through `db::config` because the bus only matters for writes and callers like `MessageBuilder` hold pools, not the manager. Pre-auth screens use the localStorage cache. Default locale is English. First-run setup asks the language in both shells — the console wizard writes `ui_locale` via `skald_core::i18n::set_default_locale` (no system bus exists there), the web setup page sends `locale` to `POST /api/setup/user`, which writes it through `GlobalConfigManager::set`. Supported locales are centralized in `skald_core::i18n::SUPPORTED_LOCALES` and enforced server-side on every write. Translated so far: chrome (sidebar/topbar), chat + approval cards, login/setup, profile, inbox; deep admin pages are still English (fallback is automatic per-key). Copy is the only place domain words may appear (§0.1).

**Plugin & backend i18n** — two seams, both keyed the same way. A plugin **page fragment** (served from its own router) localizes client-side: it ships a `web/i18n.js` module (`export default { en, it, fr }`, keys namespaced `plugin.<id>.<key>`) and calls `addStrings(dicts)` (in `web/lib/i18n.js`) once at module load to merge into the host's shared `DICTS`, then uses the same `t()`/`I18nMixin` as the app (the fragment imports them from the absolute `/lib/i18n.js` — the *same* module instance the host uses, so `t()` and `locale-changed` are shared; no endpoint, no per-locale fetch — all locales ride in the fragment, so a language switch is instant). Mobile-connector is the reference: `common.js` registers the dict + re-exports `t`, and `MobileBase extends I18nMixin(LitElement)`. **Backend-generated strings** (a plugin's HTTP error/response text, notifications) go through `core_api::i18n`: a plugin declares `Plugin::i18n() -> Vec<LocaleBundle>` (mobile-connector loads them from embedded `i18n/{en,it,fr}.json` via `include_str!`), the `PluginManager` merges every plugin's bundles once at boot into an `I18nCatalog` (`skald_core::i18n`) and injects it as `PluginContext.i18n: Arc<dyn I18nApi>`. At request time the handler resolves the caller (`Caller.user_id` from the auth layer) and calls `i18n.for_user(user_id, key, args).await` — which reads `users.locale`, runs it through the same `resolve_locale` chain, and renders `locale → en → key` with `{name}` placeholders. The frontend surfaces these already-translated: `jf()` throws the server's response text verbatim. Front and back keep **separate** tables (UI labels ≠ error strings; overlap is minimal) but share the `plugin.<id>.` namespace convention. The mechanism is general (any plugin, and eventually the core, registers the same way); only mobile-connector uses it so far.

**Role-driven interface** (§0.1 — data, not enums): `roles.attrs` JSON may carry `"ui_mode": "simple"`. `/api/auth/me` resolves it via `RoleAttrs` (`admin` is always `full`) and the sidebar renders chat + inbox only for simple-mode members; the role editor exposes it as an "Interface" select. Hiding links is never access control — routes stay capability-gated server-side. `MeResponse` also carries `locale`, `default_locale` and `encrypted`.

**Security-group picker (per-session, runtime, role-gated).** A **security-group** is a *permission bundle only* — a `tool_permission_groups` id, driving tool visibility/approval — **not** a "mode" (no system-context injection; the `RunContext.system_prompt` substrate exists but is unused by the picker). The role carries the user's **allowed set** (default `permission_group` + `attrs.permission_groups`, §0.1); a new non-project session inherits the role's default group (`sessions.rs::create` → `role_default_run_context`). The chat surface switches it **at runtime like the model pill**: `copilot.js` renders a shield pill (hidden when ≤1 group) fed by `GET /api/my/security-groups` (the caller's role set, joined with group names; `admin` → all); selecting one sends the WS control message `{type:"select_security_group", group}` (`chat-session.js::_selectGroup`, twin of `select_client`). The server (`ws.rs::handle_select_security_group_msg`) validates against the role, persists it on `chat_sessions.run_context`, updates the live handler, and **broadcasts `ServerEvent::SecurityGroupSelected`** so every open tab re-syncs (the initial state is sent on WS connect). **Enforcement is server-side** via the shared `run_context::validate_run_context_for_role` (used by both the WS path and the REST `set_session_run_context`): a non-admin may only pick a group in its role's effective set (else 403), and **every other `RunContext` field** (`system_prompt`, `allow_fs_writes`/`allow_fs_reads`, `working_directory`) is **discarded** — closing an fs-escalation hole; `admin` passes through unchanged. The role editor (`roles-page.js`) sets the default group + an allowed-groups checklist (→ `attrs.permission_groups`).

| File | Element | Notes |
| ---- | ------- | ----- |
| `copilot.js` | `<app-copilot>` | The chat surface (`_wsSource='web'`): full/dock roving layout, welcome hero empty state, privacy chip, composer with model pill, slash-command autocomplete |
| `shared/chat-page.js` | `<chat-page>` | Mobile chat (`_wsSource='mobile'`) |
| `copilot-render.js` | (helpers) | `renderMsg`, `renderTool`, `renderDiff`, etc. — shared by copilot and chat-page |
| `sidebar.js` | `<app-sidebar>` | Nav sidebar; role-driven (`ui_mode`); polls `/api/inbox` every 10 s for badge |
| `topbar.js` | `<app-topbar>` | Top nav bar; per-user avatar color hashed from the username |
| `dashboard-page.js` | `<dashboard-page>` | `#dashboard` — status hero, LLM stats charts, pending inbox, quick guide |
| `shared/file-viewer-base.js` | `FileViewerBase` (base) | Shared file-viewer engine (fetch, kind detection, markdown/PDF/SVG/LaTeX, watcher, `_renderBody`); driven by `_show`/`_hide`. Extended by desktop + mobile |
| `file-viewer-page.js` | `<file-viewer-page>` | Desktop file viewer: `FileViewerBase` + hash routing via `window.openFile(path)` → `#file_viewer?path=...` |
| `shared/file-viewer-mobile.js` | `<mobile-file-viewer-page>` | Mobile file viewer: `FileViewerBase` + prop-driven (`visible`/`path`), full-screen with back button |
| `agents.js` | `<agents-page>` | Agent discovery and config |
| `agent-inbox.js` | `<agent-inbox-page>` | Pending approvals + clarifications from background sessions |
| `approval-rules.js` | `<approval-rules-page>` | Approval rule management |
| `cron-jobs.js` | `<cron-jobs-page>` | Scheduled job management |
| `connectors.js` | `<connectors-page>` | MCP Connectors list (one row per connector): user activate/deactivate + granted globals; admin gets a **Sign-in providers** modal (OAuth client creds) + Catalog/Marketplace nav (§7/§14/§15) |
| `plugins-page.js` | `<plugins-page>` | `#plugins` — user half: granted plugins + schema-driven per-user config form |
| `plugin-catalog.js` | `<plugin-catalog>` | `#plugin-catalog` — admin status board: one card per plugin (enable toggle + health dot + Configure → `#plugin-detail`) |
| `plugin-detail.js` | `<plugin-detail>` | `#plugin-detail?id=<id>` — one plugin's admin page: instance-config form (`config_schema`) + per-user access checklist (plugin twin of `connector-detail.js`) |
| `plugin-page-host.js` | `<plugin-page-host>` | Host for plugin-contributed pages (`#plugin/<plugin_id>/<page_id>`): dynamic-imports the fragment module, registers its element, mounts it with `plugin-id` |
| `shared-folders.js` | `<shared-folders-page>` | `#shared-folders` — admin-only CRUD for on-disk shared folders (§6): create/describe/delete + per-member read-only/read-write grants; description feeds the assistant's `__SHARED_FOLDERS__` context |
| `connector-detail.js` | `<connector-detail-page>` | A connector's own page (`#connector?name=X`): env/secret form + Test, the **OAuth login panel** (sign in → paste code → complete, §15), global enable + per-user access grants |
| `shared/connector-common.js` | (helpers) | Shared Connectors vocabulary: `statusOf` (incl. `needs_login` for a pending OAuth row), `STATUS_LABEL`, schema normalization, `jf` fetch |
| `llm-providers.js` | `<llm-providers-page>` | LLM provider management |
| `models-hub.js` | `<models-hub-page>` | Models hub landing (LLM / Transcription / Image) |
| `models-llm.js` | `<models-llm-section>` | LLM model CRUD + drag-and-drop priority |
| `models-transcribe.js` | `<models-transcribe-section>` | Transcription model CRUD |
| `models-image.js` | `<models-image-section>` | Image generation model CRUD |
| `mobile-app.js` | `<mobile-app>` | Mobile app shell |
| `shared/settings-page.js` | `<settings-page>` | Mobile settings: per-user avatar, locale picker (`I18nMixin`), profile/preferences |
