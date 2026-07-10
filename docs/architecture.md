# Architecture

## Two-Layer Design

```
src/
  core/         ← headless application core (no HTTP, no Axum)
    skald/      ← Skald: Runtime context + 8 domain bundles; staged new() / shutdown()
      mod.rs        ← Skald struct (aggregate) + new() composition root + shutdown()
      runtime.rs    ← Runtime: cross-cutting context (db, buses, global_tx, shutdown, supervisor)
      bundles.rs    ← 8 domain bundles + their build() functions
      wiring.rs     ← wire() (OnceLock cycle-breakers) + spawn_background()
      supervisor.rs ← TaskSupervisor: named background-task handles, joined on shutdown
      accessors.rs  ← impl Skald: one accessor per manager (the logical API surface)
    …           ← all domain modules (db, llm, session, cron, plugin, …)
  frontend/     ← web presentation layer
    mod.rs      ← WebFrontend: wires router_factory, starts plugins, runs Axum
    server.rs   ← WebServer (Axum router, TcpListener)
    api/        ← 18 HTTP + WebSocket handlers — State<Arc<Skald>>
  core/config.rs    ← CoreConfig + DbConfig, LlmConfig, TicConfig, … (core-owned types)
  frontend/config.rs ← FrontendConfig + ServerConfig, WebConfig
  config.rs         ← Config (YAML parse only) + into_split()
  main.rs           ← thin: tracing → Config → into_split → plugins → Skald::new → WebFrontend::start → shutdown
```

`Skald` knows nothing about Axum or HTTP. It can be started headlessly. `WebFrontend` is the only component that imports Axum and constructs an HTTP server. `Skald` is no longer a God Object: its ~30 managers are grouped into a cross-cutting `Runtime` context plus eight cohesive domain bundles, and every consumer (frontend handlers, plugin context) reaches a manager through an accessor method (`skald.chat_hub()`, `skald.db()`, …) — never a public field. That accessor surface is the logical boundary a future `skald-core` crate would keep.

`Config::into_split()` produces a `CoreConfig` (db, llm, tic, cron, timezone) for `Skald::new()` and a `FrontendConfig` (server, web, timezone) for `WebFrontend::new()`. The YAML file structure is unchanged. `timezone` is cloned into both since it is used by the cron scheduler (core) and optionally by the frontend.

Plugin instances are constructed in `main.rs` as `Vec<Arc<dyn Plugin>>` and injected into `Skald::new()` — the core never depends on concrete plugin crates.

---

## Component Map

| Struct | Created by | Held as | Depends on |
| --- | --- | --- | --- |
| `SqlitePool` | `db::init_pool()` | `Arc<SqlitePool>` | — |
| `LlmManager` | `LlmManager::new()` | `Arc<LlmManager>` | `SqlitePool` |
| `McpManager` | `McpManager::new()` | `Arc<McpManager>` | `SqlitePool` |
| `TaskManager` | `TaskManager::new()` | `Arc<TaskManager>` | `SqlitePool`, `ChatSessionManager` (via OnceLock), `ChatHub` (via OnceLock) |
| `ToolRegistry` | `Tools::build()` | `Arc<ToolRegistry>` | `McpManager`, `TaskManager`, `PluginManager`, `SecretsStore` |
| `ApprovalManager` | `Interaction::build()` | `Arc<ApprovalManager>` | `SqlitePool` |
| `ClarificationManager` | `Interaction::build()` | `Arc<ClarificationManager>` | — |
| `Inbox` | `Interaction::build()` | (owned by `Interaction` bundle) | `ApprovalManager`, `ClarificationManager`, `ElicitationManager`, `ToolRegistry` |
| `ToolCatalog` | `Tools::build()` | (owned by `Tools` bundle) | `ToolRegistry`, `McpManager` |
| `ChatEventBus` | `Runtime::bootstrap()` | `Arc<ChatEventBus>` | — |
| `ContextCompactor` | `Conversation::build()` (when `llm.compaction` configured) | `Option<Arc<ContextCompactor>>` (consumed by `ChatSessionManager`) | `LlmManager`, `ChatEventBus` |
| `ChatSessionManager` | `ChatSessionManager::new()` | `Arc<ChatSessionManager>` | `SqlitePool`, `LlmManager`, `ToolRegistry`, `McpManager`, `ApprovalManager`, `ClarificationManager`, `ChatEventBus`, `ContextCompactor` |
| `ChatHub` | `ChatHub::new()` | `Arc<ChatHub>` | `SqlitePool`, `ChatSessionManager`, `ApprovalManager` |
| `TicManager` | `TicManager::new()` | `Arc<TicManager>` | `SqlitePool`, `ChatHub`, `ChatSessionManager` |
| `Skald` | `Skald::new(&core_cfg, plugins)` | `Arc<Skald>` | all of the above |
| `WebFrontend` | `WebFrontend::new(skald, &frontend_cfg)` | owned by `main` | `Arc<Skald>`, `FrontendConfig` |

### Circular Dependencies

The construction cycles cannot be expressed as a nested value tree, so the managers break them with `std::sync::OnceLock` (or `RwLock<Option<_>>`) setters. All of these setters are called in one place — the `wire()` function in `skald/wiring.rs` — run after every bundle is built and before background tasks start. Bundle structs therefore never hold references to each other; the only cross-bundle links live in the managers' `OnceLock`s.

**`TaskManager` ↔ `ChatSessionManager`**: `TaskManager` needs `ChatSessionManager` to dispatch jobs, but `ChatSessionManager` is built after `ToolRegistry` which holds `Arc<TaskManager>`. `TaskManager` is created first (in `Tasks::build`), `set_session()` is called in `wire()` once `ChatSessionManager` exists.

**`TaskManager` ↔ `ChatHub`**: Same pattern — `set_hub()` is called in `wire()`. The cron tick loop starts 30 s after `start()`, so the hub is always ready by the first real job dispatch.

**`McpManager` ↔ `ElicitationManager`**: `set_elicitation_handler()` is called in `wire()`; MCP `initialize()` is only spawned afterwards (in `spawn_background()`), so stdio servers start with a handler for server-initiated `elicitation/create`.

**`PluginManager` ↔ `Skald`**: the one `Arc<Skald>` back-reference. `PluginManager` is constructed early (in `Integrations::build`, so tools can register), then `set_skald(Arc<Skald>)` is called as the very last step of `new()`, after `Arc::new(Skald { … })`. `set_router_factory(RouterFactory)` is called by `WebFrontend::start()` before `start_enabled()`.

---

## Startup Sequence

### `main.rs`
1. Init tracing (`tracing-appender` daily rolling to `logs/`)
2. `Config::load()` → `config.into_split()` → `(CoreConfig, FrontendConfig)`
3. Build `Vec<Arc<dyn Plugin>>` — all plugin instances constructed here
4. `Skald::new(&core_cfg, plugins)` — see sequence below
5. `WebFrontend::new(skald, &frontend_cfg)` + `.start()` — see sequence below
6. Await `ctrl_c`
7. `skald.shutdown()` + `handle.shutdown()`

### Inside `Skald::new(&core_cfg, plugins)`

`new()` is a **staged composition root**: it builds the `Runtime` context, then each domain bundle in dependency order via its own `Bundle::build(...)`, then resolves cycles and starts background tasks. (`db::init_pool()` runs earlier, in `main.rs`, and the pool is passed in.)

1. `agents::discover()` — scans `agents/*/` for `meta.json` + `AGENT.md` (logged only)
2. `Runtime::bootstrap(pool)` — `GlobalConfigManager`, `SystemEventBus`, `ChatEventBus`, the `GlobalEvent` broadcast channel (`global_tx`), `CancellationToken`, `TaskSupervisor`
3. `Models::build` — `ProviderRegistry` (+6 built-in providers), `LlmManager`, `SecretsStore`, `MemoryManager`
4. `Media::build` — `ImageGeneratorManager`, `TranscribeManager`, `TtsManager`
5. `Integrations::build` — `McpManager` (NOT yet initialized), `PluginManager` (plugins registered, not started)
6. `Tasks::build` — `TaskManager` (scheduler, not started), `ProjectManager`, `ProjectTicketManager` — before `Tools` so cron tools can capture cron
7. `Tools::build` — `ToolRegistry` (all built-in tools, capturing mcp/plugins/cron/secrets + mobile-connector tools), `ToolCatalog`, `LlmCommandManager`
8. `Interaction::build` — `ApprovalManager` (+ 4 non-fatal `seed_*`), `ClarificationManager`, `ElicitationManager`, `Inbox`
9. `Conversation::build` — `RunContextManager` (+ seed), `ContextCompactor` (if configured), `ChatSessionManager`, `ChatHub` (+ `register("web"/"talk")`), `TicManager`
10. `Infra::build` — `LatexCompiler`, `LocationManager`, `remote` slot (`None`)
11. `wire(...)` — all `OnceLock` cycle-breakers in one place (`cron.set_session/set_hub/set_self_arc`, `ticket.set_task_manager`, `chat_hub.set_task_mgr`, `mcp.set_elicitation_handler`)
12. `spawn_background(...)` — every long-lived task, each registered by name with the supervisor: `llm-log-cleanup`, `session-cancel`, `mcp-init` (`mcp.initialize()`), `cron`, `ticket-listener`, `tic`
13. `Arc::new(Skald { rt, models, media, tools, integrations, tasks, conversation, interaction, infra })`
14. `skald.plugin_manager().set_skald(Arc::clone(&skald))` — the one `Arc<Skald>` back-reference, last

### Inside `WebFrontend::start()`
28. `plugin_manager.set_router_factory(factory)` — provides Axum router factory to plugins
29. `plugin_manager.set_web_port(port)` — provides HTTP port to plugins (e.g. Tailscale)
30. `plugin_manager.start_enabled()` — starts Telegram and other enabled plugins
31. `plugin_manager.start_config_watcher(shutdown_token)` — polls DB every 30 s
32. `WebServer::start(addr)` — Axum HTTP+WS server begins listening

---

## Request Lifecycle

1. Client opens WebSocket: `GET /api/ws`
2. `handle_socket()` gets or creates `ChatSessionHandler` via `ChatHub::session_handler("web")`
3. Client sends `ClientMessage` JSON over WS
4. `ChatHub::send_message("web", prompt, opts)` **enqueues** the message on the source's inbox and returns; it does not run the turn inline (see *Per-source inbox* in [session.md](session.md))
5. The source's single consumer task drains the inbox, **coalescing** any messages that piled up during an in-flight turn into one prompt (joined by blank lines), and calls `handler.handle_message(...)`
6. `handle_message` acquires `processing: Mutex<()>` (one at a time per session)
7. `run_agent_turn` is a thin round orchestrator (up to `max_tool_rounds` rounds) that delegates each step to focused collaborators (see [session.md](session.md#run_agent_turn-inner-loop)):
   - Build context: `build_openai_messages()` → system prompt + history + tool results
   - Apply permission-group visibility filter (hide tools `Deny`d for the session's run-context group)
   - `call_llm_round()` — one LLM call + automatic model fallback on retriable errors
   - `LlmTurn::Message` → send `Done` event, exit loop
   - `LlmTurn::ToolCalls` → for each call, `handle_tool_call()`:
     - `effective_args()` (working-dir injection) → `run_approval_gate()` → optional `PendingWrite`, wait for user
     - `execute_tool_call()` → send `ToolStart`; `call_agent` recurses via `dispatch_sub_agent`
     - `record_tool_outcome()` → send `ToolDone` / `ToolError` / `ToolCancelled`
   - All events are emitted through the `TurnEmitter` seam (`emitter.rs`)
8. Main loop sends `Done` event with final content and token counts

---

## Notification Flow (background)

```text
MCP server stdout (JSON-RPC notification, no id field)
  → McpServer reader loop (src/core/mcp/server.rs)
  → notification_tx (mpsc::UnboundedSender)
  → McpManager::notification_consumer
  → db::mcp_events::insert(source, method, payload)

[every tic.interval_secs (default 900 s) — TicManager::run_tick()]
  → mcp_events::pending_limited(tic.batch_size)
  → mcp_events::mark_processed(ids)
  → build_prompt(events)
  → ChatHub::send_message("tic", prompt)   ← ephemeral session
  → TIC agent runs, calls notify(briefing)
  → ChatHub::notify_sync → mpsc channel

[ChatHub::notification_consumer]
  → batching window (200 ms)
  → appends synthetic Assistant message to chat_history (reasoning_content + is_synthetic=true)
  → inserts chat_llm_tools row for read_notification (status='done', result=[briefings])
  → calls hub.resume(home_source)
  → resume_turn picks up the synthetic tool call, runs the LLM loop
  → user sees assistant briefing in home conversation
```

---

## Skald Fields (bundle model)

`Skald` holds one cross-cutting `Runtime` context plus eight domain bundles — all private. Consumers reach managers through **accessor methods** named after the manager (`skald.chat_hub()`, `skald.db()`, `skald.approval()`, …), defined in `skald/accessors.rs`. The frontend uses these via `State<Arc<Skald>>`; the plugin context (`PluginManager::build_context`) uses the same surface. High-level facets like `Inbox` and `ToolCatalog` encapsulate several underlying managers behind a simpler API.

| Bundle | Field | Members (accessor → manager) |
| --- | --- | --- |
| `Runtime` | `rt` | `db` → `SqlitePool`, `config` → `GlobalConfigManager`, `config_properties`, `system_bus` → `SystemEventBus`, `event_bus` → `ChatEventBus`, `global_tx` (`GlobalEvent` sender), `shutdown_token`, `supervisor` → `TaskSupervisor` |
| `Models` | `models` | `provider_registry`, `llm_manager`, `secrets`, `memory_manager` |
| `Media` | `media` | `image_generator_manager`, `transcribe_manager`, `tts_manager` |
| `Tools` | `tools` | `tools` → `ToolRegistry`, `catalog` → `ToolCatalog`, `command_manager` → `LlmCommandManager` |
| `Integrations` | `integrations` | `mcp` → `McpManager`, `plugin_manager` → `PluginManager` |
| `Tasks` | `tasks` | `cron` → `TaskManager`, `projects` → `ProjectManager`, `ticket_manager` → `ProjectTicketManager` |
| `Conversation` | `conversation` | `manager` → `ChatSessionManager`, `chat_hub` → `ChatHub`, `run_context_manager` → `RunContextManager`, `tic_manager` → `TicManager` |
| `Interaction` | `interaction` | `approval` → `ApprovalManager`, `inbox` → `Inbox`, `clarification` → `ClarificationManager`, `elicitation` → `ElicitationManager` |
| `Infra` | `infra` | `latex_compiler` → `LatexCompiler`, `location_manager` → `LocationManager`, `remote` → `Arc<RwLock<Option<Arc<dyn RemoteAccess>>>>` |

`TicManager` lives in `Conversation` (not `Tasks`) because it is constructed from and drives the conversation stack (session manager + chat hub + run context); this keeps every bundle a single-shot `build()` with no two-phase init.

---

## Graceful Shutdown

On SIGINT, `main.rs` executes:

1. `skald.shutdown()`:
   - `rt.shutdown_token.cancel()` — signals all background loops to exit their `select!`
   - `rt.supervisor.join_all(10 s)` — awaits every supervised task against a shared deadline, logging any laggard **by name**
   - `plugin_manager.stop_all()`
2. `handle.shutdown()` — drains and closes the Axum HTTP server

Tasks registered with the `TaskSupervisor` (spawned in `spawn_background`, joined on shutdown, listed by their supervisor name):

| Supervisor name | Source |
| --- | --- |
| `cron` | `src/core/cron/mod.rs` (`TaskManager::start`) |
| `ticket-listener` | `src/core/projects/tickets.rs` |
| `tic` | `src/core/tic/mod.rs` |
| `session-cancel` | `src/core/skald/wiring.rs` |
| `mcp-init` | `src/core/skald/wiring.rs` → `McpManager::initialize` |
| `llm-log-cleanup` | `src/core/db/llm_requests/cleanup.rs` |

Other tasks are spawned inside their managers and honor `shutdown_token.cancelled()` but are not (yet) registered with the supervisor — cancellation stops them, but they are not individually joined: `PluginManager` config watcher (`src/core/plugin/mod.rs`), `McpManager` notification consumer (`src/core/mcp/mod.rs`), `ChatHub` notification consumer (`src/core/chat_hub/mod.rs`), `TtsManager`/`TranscribeManager` reload watchers (`src/core/{tts,transcribe}/manager.rs`).

---

## Workspace Crates

The binary depends on several independent library crates in `crates/`. Each crate has no dependency on the main `skald` crate and can be published or reused standalone.

| Crate | Path | Purpose |
| --- | --- | --- |
| `core-api` | `crates/core-api/` | Shared types and traits: `ServerEvent`, `GlobalEvent`, `InterfaceTool`, `SendMessageOptions`, `ChatHubApi` trait |
| `llm-client` | `crates/llm-client/` | LLM client abstraction (OpenAI-compat, Anthropic, Ollama) |
| `mcp-client` | `crates/mcp-client/` | MCP client (JSON-RPC over stdio/SSE) |
| `honcho-client` | `crates/honcho-client/` | Honcho long-term memory HTTP client |

### `core-api` — plugin extraction boundary

`core-api` is the designated contract crate for plugin independence. A plugin that depends only on `core-api` (instead of the full main crate) can be extracted into its own workspace member without circular dependencies.

See [crates/workspace.md](crates/workspace.md) for the full extraction roadmap.

### Future: `skald-core` crate

`src/core/` is designed as a stepping stone toward extracting the headless core into a standalone `crates/skald-core/` crate. The accessor facade in `skald/accessors.rs` is the seam: the frontend and plugin context already consume `Skald` only through those methods, never its fields. When the extraction happens:
- `src/core/` moves to `crates/skald-core/src/`
- the `skald/accessors.rs` methods are promoted to a `pub trait SkaldApi` (they already have that shape)
- the two raw-SQL leaks the frontend still has (`skald.db()` used directly for a handful of `sqlx::query*` calls in `sessions.rs`/`dev.rs`/`stats.rs`, and `skald.system_bus()` for two `.send()` sites) must be closed behind repository/intent methods first
- `src/frontend/` depends on `skald-core` as a path dependency

---

## When to Update This File

- A new manager is added to a `Skald` bundle (add its accessor + update the bundle table)
- A bundle is added, removed, or a manager moves between bundles
- The staged construction in `Skald::new()` / a `Bundle::build()` or `WebFrontend::start()` changes
- The request lifecycle changes (new event type, new loop behavior)
- A new circular dependency and its `wire()` resolution is introduced
- A background task is added to (or removed from) the `TaskSupervisor`
- A new workspace crate is added
