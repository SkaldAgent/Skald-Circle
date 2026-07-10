# Skald — Documentation

## Documentation Rule (MANDATORY)

**Every source code change — made by a human or by an LLM — must be accompanied by an update to the relevant doc file(s). No exception.**

This includes:

- Adding or removing a tool → update [tools.md](tools.md)
- Changing a table schema → update [database.md](database.md)
- Modifying the approval gate or tool loop → update [session.md](session.md)
- Adding a new agent → update [agents.md](agents.md)
- Any change to the WS protocol → update [frontend.md](frontend.md)
- Changing the project/ticket lifecycle or project chats → update [projects.md](projects.md)
- Changing ChatHub dispatch, the per-source inbox/coalescing, or `/stop`/`/clear` semantics → update [chat-hub.md](chat-hub.md)
- Adding or changing custom slash commands, the command manager, or the composer autocomplete → update [commands.md](commands.md)
- Changing the `desktop` feature, Tauri wiring, tray/menu, window policy, or `bootstrap_data_dir` → update [desktop.md](desktop.md)

---

## Key paths (agent: read this first)

| Resource | Default path | Override |
| --- | --- | --- |
| **SQLite database** | `./database.db` | `db.path` in `config.yml` |
| **Config file** | `./config.yml` | — (copy from `default.config.yaml`) |
| **Secrets folder** | `./secrets/` | — |
| **Model cache** | `./models/` | — |
| **Log files** | `./logs/` | — |
| **Static web assets** | `./web/` | `web.static_dir` in `config.yml` |

When looking for the database, **always use `./database.db`** unless `config.yml` says otherwise.

---

## Project Summary

A local chat server (Axum + Tokio + SQLite) where an LLM handles user queries via tool calls. The app can rewrite and restart its own source code autonomously. Multiple specialized agents collaborate via a recursive sub-agent system. External tools are integrated via MCP (Model Context Protocol). Entry point: `run.sh`.

## Workspace Structure

The project is a Cargo workspace. Extracted crates live in `crates/`:

| Crate | Path | Notes |
| --- | --- | --- |
| `skald` | `.` (root) | Main application binary |
| `core-api` | `crates/core-api/` | Shared types and traits: `ServerEvent`, `GlobalEvent`, `ChatHubApi`, `ApprovalApi`, `InboxApi`, `Tool`, `Memory`, `ChatbotClient`, `Transcribe`, `TextToSpeech`, `ImageGenerate`, `SecretsApi`, `LocationManager`, `InterfaceTool`, `Plugin`, `PluginContext`, `ApiProvider`, `ApiProviderRegistry`, `RemoteAccess`. Also owns all DB record types: `LlmProviderRecord`, `LlmModelRecord`, `TtsModelRecord`, `TranscribeModelRecord`, `ImageGenerateModelRecord`. |
| `llm-client` | `crates/llm-client/` | OpenAI-compatible, Anthropic, Ollama, LmStudio implementations of `ChatbotClient`. Depends on `core-api` and re-exports the trait and associated types for backward compatibility. |
| `mcp-client` | `crates/mcp-client/` | MCP protocol layer: `McpServer` (stdio), `McpHttpServer`, `McpServerClient` trait, config types |
| `honcho-client` | `crates/honcho-client/` | Honcho v3 REST API client — zero dependencies on the main crate |
| `plugin-honcho` | `crates/plugin-honcho/` | Honcho memory sink plugin |
| `plugin-tailscale-remote` | `crates/plugin-tailscale-remote/` | Remote connectivity via Tailscale mesh |
| `plugin-transcribe-whisper-local` | `crates/plugin-transcribe-whisper-local/` | Local STT via whisper.cpp (Metal-accelerated) |
| `plugin-telegram-bot` | `crates/plugin-telegram-bot/` | Private Telegram bot interface |
| `plugin-tts-orpheus-3b` | `crates/plugin-tts-orpheus-3b/` | Local TTS via Orpheus 3B (Python subprocess) |
| `plugin-tts-kokoro` | `crates/plugin-tts-kokoro/` | Local TTS via Kokoro ONNX (lightweight, multilingual) |
| `skald-relay-common` | `crates/skald-relay-common/` | Shared crypto + v2 protobuf frame types for the relay and mobile-connector plugin; owns the `gen-vectors` reference generator. Implements v2 binary transport (presence, live channel). No axum/tokio/Skald deps. |
| `skald-relay-server` | `crates/skald-relay-server/` | Zero-trust store-and-forward relay + push bridge (iOS/Android remote control). v2 binary transport (protobuf), presence tracking, live channel route-or-fail. Depends on `skald-relay-common`. Live APNs sender behind the `push-live` cargo feature (see [crates/workspace.md](crates/workspace.md)). |
| `plugin-mobile-connector` | `crates/plugin-mobile-connector/` | Agent end of the v2 relay protocol: bridges the Inbox to mobile apps over WebSocket, E2E encrypted. Handles presence, live inbox pulls, pairing. See [plugins/mobile-connector.md](plugins/mobile-connector.md). |

To add a new extracted crate: create `crates/<name>/`, add it to the `[workspace].members` list in the root `Cargo.toml`, then add a `path` dependency in `[dependencies]`.

---

## Module Map

| Source path | Role | Doc |
| --- | --- | --- |
| `src/main.rs` | Thin entry point: tracing → `Config` → `into_split` → plugins → `Skald::new` → `WebFrontend::start` → shutdown | [architecture.md](architecture.md) |
| `src/desktop/mod.rs` | Tauri shell — only compiled under `--features desktop`. Wraps `run_backend()` in a Tauri event loop with a system-tray icon (`Open` / `Quit`), creates the main `WebviewWindow` from `config.yml`'s `server.port`, and handles graceful shutdown | [desktop.md](desktop.md) |
| `src/core/skald.rs` | `Skald` — headless application core; owns all managers; `new(cfg, plugins)` / `shutdown()` | [architecture.md](architecture.md) |
| `src/core/config.rs` | `CoreConfig` + core config types (`DbConfig`, `LlmConfig`, `TicConfig`, `CompactionConfig`, …) | [architecture.md](architecture.md) |
| `src/frontend/config.rs` | `FrontendConfig` (`ServerConfig`, `WebConfig`, `timezone`) | [architecture.md](architecture.md) |
| `src/core/session/handler/` | Core LLM loop, tool dispatch, approval | [session.md](session.md) |
| `src/core/session/handler/message_builder.rs` | `MessageBuilder` — pure service for building OpenAI message arrays, testable in isolation | [session.md](session.md) |
| `src/core/session/manager.rs` | Session factory | [session.md](session.md) |
| `src/core/agents.rs` | Agent discovery, prompt loading | [agents.md](agents.md) |
| `src/core/command/mod.rs` | `LlmCommandManager` — file-based custom slash commands (`commands/<name>/`) | [commands.md](commands.md) |
| `src/core/tools/` | Built-in tool registry | [tools.md](tools.md) |
| `src/core/tools/tool_names.rs` | Centralised tool name constants (`CALL_AGENT`, `RESTART`, …) | [tools.md](tools.md) |
| `src/core/tool_catalog.rs` | `ToolCatalog`: unified listing façade for built-in + MCP tools (wraps ToolRegistry + McpManager); `AllTools` response includes `mcp_servers: HashMap<String, McpServerMeta>` (friendly name + description per MCP server) | [tools.md](tools.md) |
| `src/core/provider/` | `ProviderRegistry` (implements `ApiProviderRegistry`) — thin wrapper around `core-api::provider`. All types re-exported for internal use. | [llm-clients.md](llm-clients.md) |
| `src/core/service_manager.rs` | `ServiceManager` trait — lightweight umbrella for all model managers | [llm-clients.md](llm-clients.md) |
| `src/core/chatbot/` | LLM provider clients | [llm-clients.md](llm-clients.md) |
| `src/core/llm/manager.rs` | LLM selection, health tracking | [llm-clients.md](llm-clients.md) |
| `src/core/chat_event_bus.rs` | In-process broadcast bus for chat turns and compaction events | [chat-event-bus.md](chat-event-bus.md) |
| `src/core/compactor.rs` | Context compaction — summarises old history to reduce token usage | [compaction.md](compaction.md) |
| `src/core/memory/` | Pluggable long-term memory layer (trait + manager) | [memory.md](memory.md) |
| `src/core/chat_hub/` | Central chat orchestrator for **interactive, user-facing sessions only** (web, mobile, project chats — one persistent session per source via the `sources` table); per-source coalescing inbox; notification pipeline. `provision_session(source, agent_id, rc, reset)` is the single source→session entry point; `clear()` is a thin `main`-agent wrapper over it. **Not** for background agents (cron/TIC/sub-agents → `TaskManager`/`ChatSessionManager`) | [chat-hub.md](chat-hub.md) |
| `src/core/tic/` | Background MCP event processor (TicManager) | [architecture.md](architecture.md) |
| `src/core/mcp/` | MCP server management, push notification ingestion | [mcp.md](mcp.md) |
| `src/core/cron/` | Scheduled job scheduler | [cron.md](cron.md) |
| `src/core/plugin/` | Plugin system (PluginManager) | [plugins.md](plugins.md) |
| `src/core/secrets.rs` | SecretsStore — centralised token/key store over SQLite | [secrets.md](secrets.md) |
| `src/core/transcribe/` | TranscribeManager, OpenAiAudioTranscriber, ElevenLabsTranscriber. Traits and record types re-exported from `core-api`. | [providers/transcribe.md](providers/transcribe.md) |
| `src/core/tts/` | TtsManager (DB-backed + plugin slots), OpenAiTtsSynthesiser, ElevenLabsTtsSynthesiser. Traits and record types re-exported from `core-api`. | [providers/tts.md](providers/tts.md) |
| `src/core/image_generate/` | ImageGenerate trait, ImageGeneratorManager (DB-backed + plugin slots), OpenRouterImageGenerator | [providers/image.md](providers/image.md) |
| `src/core/run_context/mod.rs` | `RunContext` domain object: fields `security_group`, `system_prompt`, `allow_fs_writes`, `working_directory` + applicative methods `tool_group_id()`, `extra_system_prompt()`, `effective_working_dir()`, `is_write_allowed()`. `RunContextManager`: permission group CRUD; `set_session_run_context`; `duplicate_group`; `check_tool_visibility`. | [approval/index.md](approval/index.md) |
| `src/core/projects/mod.rs` | `ProjectManager` — CRUD for projects (filesystem-linked, ordered by `updated_at`). Free fn `build_runtime_run_context(project, base)` layers project-runtime fields (`working_directory = project.path`, `allow_fs_writes` for the project tree + `{skald_cwd}/data`, project-context system prompt fragments) over an optional base RC — shared by ticket jobs and interactive project chats | [projects.md](projects.md) |
| `src/core/projects/tickets.rs` | `ProjectTicketManager` — CRUD + lifecycle for project tickets (`start`, `on_job_completed`, `reset`); `start()` resolves the base `RunContext` (ticket override → project static config) and delegates to `projects::build_runtime_run_context` for the runtime fields | [projects.md](projects.md) |
| `src/core/inbox.rs` | `Inbox`: unified façade for pending approvals + clarifications (wraps ApprovalManager, ClarificationManager, ChatHub) | [approval/index.md](approval/index.md) |
| `src/core/db/` | SQLite schema and queries | [database.md](database.md) |
| `src/core/events.rs` | WS protocol types | [frontend.md](frontend.md) |
| `src/frontend/mod.rs` | `WebFrontend` — wires `router_factory`, starts plugins, runs Axum | [architecture.md](architecture.md) |
| `src/frontend/server.rs` | `WebServer` — Axum router, TcpListener, `WebServerHandle` | [architecture.md](architecture.md) |
| `src/frontend/api/` | HTTP + WebSocket handlers — `State<Arc<Skald>>` | [frontend.md](frontend.md) |
| `src/frontend/api/projects.rs` | REST CRUD for projects and tickets — `GET/POST /api/projects`, `GET/PUT/DELETE /api/projects/{id}`, tickets sub-routes, `start`/`reset` lifecycle. `POST /api/projects/{id}/session` opens/resumes the project chat (source `project-{id}`, agent `project-coordinator`). `provisioning_for_source(skald, source)` maps a source → (agent, RunContext) and is reused by `POST /api/sessions` so project resets recreate with the coordinator | [projects.md](projects.md) |
| `src/config.rs` | `Config` (YAML aggregate: `ServerConfig`, `WebConfig` + re-exports from `core::config`) + `Config::into_split()` + `bootstrap_data_dir()` (cwd relocation under the `desktop` feature) | [logging-config.md](logging-config.md), [desktop.md](desktop.md) |
| `crates/plugin-honcho/` | Honcho memory sink (standalone crate) | [honcho.md](honcho.md) |
| `crates/plugin-tailscale-remote/` | Remote connectivity via Tailscale mesh (standalone crate) | [remote.md](remote.md) |
| `crates/plugin-transcribe-whisper-local/` | Local STT via whisper.cpp (standalone crate) | [whisper-local.md](whisper-local.md) |
| `crates/plugin-telegram-bot/` | Private Telegram bot (standalone crate) | [plugins/telegram.md](plugins/telegram.md) |
| `crates/plugin-tts-orpheus-3b/` | Orpheus TTS 3B — local TTS via Python subprocess (standalone crate) | [providers/tts.md](providers/tts.md) |
| `crates/plugin-tts-kokoro/` | Kokoro ONNX — lightweight local TTS, multilingual (standalone crate) | [providers/tts.md](providers/tts.md) |
| `crates/honcho-client/` | Honcho v3 REST API client (standalone crate) | [honcho.md](honcho.md) |
| `web/components/` | Lit frontend components | [frontend.md](frontend.md) |
| `run.sh` | Supervisor loop | [self-rewriting.md](self-rewriting.md) |

---

## Critical Constants

| Constant | Value | Location |
| --- | --- | --- |
| `MAX_AGENT_DEPTH` | **5** | `src/core/session/handler/mod.rs` |
| `DEFAULT_MAX_TOOL_ROUNDS` | **20** | `src/core/session/handler/mod.rs` |
| `FAILURE_DEGRADED` | **3** consecutive failures | `src/core/llm/manager.rs` |
| `FAILURE_DOWN` | **5** consecutive failures | `src/core/llm/manager.rs` |
| Cron scheduler tick | **30 s** | `src/core/cron/mod.rs` |
| Cron fire-check window | **90 s** | `src/core/cron/mod.rs` |
| MCP startup timeout | **120 s** | `src/core/mcp/mod.rs` |
| TIC tick interval | **900 s** default | `config.yml` → `tic.interval_secs`; overridable at runtime via `tic.interval_minutes` DB key |
| TIC batch size | **50 events** default | `config.yml` → `tic.batch_size` |
| Notification batch window | **200 ms** | `src/core/chat_hub/mod.rs` |

---

## Navigation

### Core Architecture

- [architecture.md](architecture.md) — component wiring, startup sequence, request lifecycle
- [desktop.md](desktop.md) — Tauri desktop bundle: system tray, window policy, two-deployment-shape model, build instructions
- [build-and-distribution.md](build-and-distribution.md) — portable static binary: rustls+ring (no OpenSSL/aws-lc), musl build recipe, cargo features, runtime deps
- [self-rewriting.md](self-rewriting.md) — restart mechanism, safe self-modification workflow
- [database.md](database.md) — SQLite schema, migration pattern
- [logging-config.md](logging-config.md) — log levels, config.yml full reference

### Session & LLM Loop

- [session.md](session.md) — ChatSessionHandler, message flow, approval gate integration
- [session/run-context.md](session/run-context.md) — RunContext: permissions, system prompt, file authorization, working directory
- [llm-clients.md](llm-clients.md) — ChatbotClient trait, LlmManager, ApiProvider, ProviderRegistry, AUTO selection
- [compaction.md](compaction.md) — context compaction: trigger, summarisation flow, DB schema, config
- [memory.md](memory.md) — Memory trait, MemoryManager, integration in the LLM loop

### Approval & Permissions

- [approval/index.md](approval/index.md) — ApprovalManager: human-in-the-loop, rules, pending approvals, session bypass; tool visibility filtering; group duplication
- [session/run-context.md](session/run-context.md) — RunContext fields and integration (single source of truth)

### Tools & Agents

- [agents.md](agents.md) — agent discovery, meta.json, call_agent, depth limit
- [commands.md](commands.md) — custom `/command` shortcuts: file layout, `{{args}}`, dual-view, autocomplete
- [tools.md](tools.md) — Tool trait, ToolRegistry, built-in catalogue
- [chat-event-bus.md](chat-event-bus.md) — ChatEventBus, event types, publication rules, adding consumers
- [cron.md](cron.md) — TaskManager, task kinds (cron/sync/async), 7-field cron syntax, job lifecycle, async result delivery

### Model Providers

- [llm-clients.md](llm-clients.md) — LLM client trait and selection
- [providers/tts.md](providers/tts.md) — Text-to-Speech: trait, manager, provider catalogue, tts_models DB table
- [providers/transcribe.md](providers/transcribe.md) — Cloud STT via OpenAI-compatible audio API, transcribe_models DB table
- [providers/image.md](providers/image.md) — Image generation: trait, manager, async task system, LLM tools, REST endpoint

### MCP (Model Context Protocol)

- [mcp.md](mcp.md) — McpManager, transports, naming convention, enable/disable, integration
- [mcp/specs/index.md](mcp/specs/index.md) — Specification reference: one file per official MCP spec revision (2024-11-05 Legacy → 2025-06-18 Stable → 2025-11-25 Latest Stable → 2026-07-28 draft), with a comparative feature matrix
- [mcp/servers/gmail.md](mcp/servers/gmail.md) — Gmail read+modify MCP server (custom Python)
- [mcp/servers/gcal.md](mcp/servers/gcal.md) — Google Calendar read-only MCP server (custom Python)
- [mcp/servers/gmaps.md](mcp/servers/gmaps.md) — Google Maps transit/directions MCP server (custom Python)
- [mcp/servers/whatsapp.md](mcp/servers/whatsapp.md) — WhatsApp read+send MCP server (custom Node.js)

### Plugin System

- [plugins.md](plugins.md) — Plugin trait, PluginManager, HTTP router integration
- [plugins/honcho.md](plugins/honcho.md) — Honcho memory plugin: setup, config, filtering, lifecycle
- [plugins/mobile-connector.md](plugins/mobile-connector.md) — Mobile app relay bridge, E2E encryption, Inbox synchronization
- [plugins/telegram.md](plugins/telegram.md) — Telegram bot setup, pairing, whitelist, HITL approval
- [plugins/whisper-local.md](plugins/whisper-local.md) — Local STT via whisper.cpp, model setup, TranscribeManager integration
- [plugins/remote.md](plugins/remote.md) — Tailscale mesh remote connectivity

### Relay Protocol (Mobile Remote Control)

- [relay/index.md](relay/index.md) — Architecture, actors, threat model, encoding conventions
- [relay/crypto.md](relay/crypto.md) — Crypto contract: seed, key derivation, ECDH, HKDF, AES-256-GCM, anti-replay
- [relay/relay-protocol.md](relay/relay-protocol.md) — WebSocket protocol: protobuf transport, auth, pairing, live channel, presence
- [relay/framing.md](relay/framing.md) — E2E plaintext framing: version byte + optional zlib compression
- [relay/payloads.md](relay/payloads.md) — E2E payload schemas: inbox_update, approval_response, clarification_response, …
- [relay/describe-and-push.md](relay/describe-and-push.md) — Approval rendering: summary + structured blocks, push delivery model
- [relay/server.md](relay/server.md) — Relay server implementation: zero-trust, store-and-forward, APNs/FCM bridge, deploy
- [relay/test-vectors.md](relay/test-vectors.md) — Crypto test vectors + reference generator for byte-for-byte interop

### Projects & Tickets

- [projects.md](projects.md) — Projects subsystem: kanban tickets, lifecycle, `build_runtime_run_context`, interactive project chats

### Frontend & Notifications

- [frontend.md](frontend.md) — WebSocket protocol, ServerEvent types, Lit components
- [notifications.md](notifications.md) — Notification system: `read_notification` tool, synthetic injection flow, `data/notifications.md` format

### Infrastructure & Security

- [secrets.md](secrets.md) — SecretsApi trait, SecretsStore, well-known keys, security notes
- [crates/workspace.md](crates/workspace.md) — Workspace crate catalogue, `core-api` module reference, plugin extraction roadmap

### Miscellaneous

- [skills.md](skills.md) — Skills system: reusable Python capability packages

## When to Update This File

- A new source module is added or removed
- A critical constant changes
- A new doc file is added to `docs/`
