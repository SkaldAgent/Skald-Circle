# Logging & Configuration

## Logging Setup

Log files are written to `logs/` using `tracing-appender` with daily rotation:

```
logs/skald.log.YYYY-MM-DD
```

A new file is created each day. The non-blocking writer is initialized in `main()` and the `_log_guard` is kept alive for the full process lifetime to ensure all buffered logs are flushed on shutdown.

The subscriber has **two layers** (`main.rs`):

| Layer | Writer | Filter | Format |
|---|---|---|---|
| File | `logs/skald.log` (daily) | `EnvFilter` (`RUST_LOG`, default `info`) | full structured (timestamp, level, target, fields) |
| Stdout | terminal | `boot` target only | minimal — message only, failures in red |

**Runtime output goes only to the file.** Stdout carries just the curated
**bootstrap** lines (see below); once the app is up nothing else is printed there.

---

## Bootstrap output (stdout)

During startup a small, ordered set of human-readable lines is printed to stdout
so you can see at a glance how the app is configured and how it is coming up:

```
skald v0.1.0 — starting
› Database ready (schema v16)
› MCP servers — connecting to 18 in background
  ✓ codebase-memory (14 tools)
› Plugins — 6 active, 1 failed, 2 available
  ✓ honcho, telegram, comfyui, elevenlabs, mobile-connector, whisper_local
  ✗ remote_connectivity — creating tailscale device
  ○ orpheus_tts_3b, kokoro_tts
✅ Ready — http://localhost:3000
  ✓ gcal (8 tools)
  ...
```

These are emitted via the helpers in `src/boot.rs` (`title`, `section`, `ok`,
`off`, `fail`, `ready`) on the `boot` tracing target. They are rendered by a
dedicated stdout layer that:

- shows **only** the `boot` target (it ignores `RUST_LOG`, so bootstrap output
  always appears);
- strips timestamps/levels/targets and colours failures red (ANSI only on a TTY);
- still lets the same lines reach the **file** log as a high-level startup trace.

Note the glyph convention: `✓` started/connected, `✗` failed (with reason),
`○` available but disabled, `›` phase header, `✅` ready.

**MCP servers connect asynchronously** and do not block startup, so their `✓`/`✗`
lines stream in as each server responds — some may appear *after* the `✅ Ready`
line. The app is usable as soon as `Ready` prints (HTTP listening); MCP tools
become available as their servers connect.

To add a bootstrap line from anywhere in the binary crate, call
`crate::boot::section("…")` (or `ok`/`fail`/`off`). Keep them few and targeted —
this is a curated summary, not a log.

---

## Log Levels and RUST_LOG

Default level: **`info`**

Override with the `RUST_LOG` env var:

| Example | Effect |
|---|---|
| `RUST_LOG=info` | Default: info and above |
| `RUST_LOG=skald=debug,info` | Debug for this crate, info for dependencies |
| `RUST_LOG=trace` | Everything (very verbose) |

Level semantics:

| Level | Use for |
|---|---|
| `ERROR` | Failures requiring a code or config fix (config load failure, DB init error, LLM loop exhausted) |
| `WARN` | Non-critical anomalies to fix eventually (malformed API response, agent skipped) |
| `INFO` | Normal significant events (server started, session opened, job executed, response tokens) |
| `DEBUG` | Per-operation lifecycle (request sent to LLM, tool dispatched, context built) |
| `TRACE` | Fine-grained internals (round counters, full request bodies, message arrays) |

A dropped WebSocket connection, a cancelled request, or a completed session are **INFO** at most — they are expected runtime events, not errors.

---

## config.yml Structure

Loaded by `Config::load()` at startup. Copied from `default.config.yaml` if `config.yml` does not exist. **Never commit `config.yml`** — it may contain API keys.

| Section | Key | Type | Default | Notes |
|---|---|---|---|---|
| `server` | `host` | string | `127.0.0.1` | Bind address |
| `server` | `port` | u16 | `3000` | HTTP/WS port |
| `web` | `static_dir` | string | `./web` | Path to static frontend files |
| `db` | `path` | string | `./database.db` | SQLite file path |
| `llm` | `max_history_messages` | usize | `30` | Max messages kept per context window. Ignored when `compaction` is configured — the compactor manages the token budget instead. |
| `llm` | `max_tool_rounds` | usize? | `20` | Max tool-call rounds per message; falls back to `DEFAULT_MAX_TOOL_ROUNDS` |
| `llm.requests_log` | `enabled` | bool | `false` | Log every LLM call to the `llm_requests` table. **Disabling this also disables home-page LLM statistics.** |
| `llm.requests_log` | `request_payload_save` | bool | `true` | Persist request JSON (can be hundreds of KB per call) |
| `llm.requests_log` | `response_payload_save` | bool | `true` | Persist response JSON |
| `llm.requests_log` | `request_header_save` | bool | `true` | Persist request HTTP headers (api-key always redacted) |
| `llm.requests_log` | `response_header_save` | bool | `true` | Persist response HTTP headers |
| `llm.requests_log` | `cleanup_request_payload_after` | u32? | `null` | Set `request_json = ''` for rows older than N days |
| `llm.requests_log` | `cleanup_response_payload_after` | u32? | `null` | Set `response_json = NULL` for rows older than N days |
| `llm.requests_log` | `cleanup_headers_after` | u32? | `null` | Null out both header columns for rows older than N days |
| `llm.requests_log` | `cleanup_rows_after` | u32? | `null` | Physically delete rows older than N days (`null` = keep forever) |

The `llm.clients` block in `config.yml` is for reference only — actual runtime LLM providers and models are stored in the DB and managed via the UI or API.

---

## LLM Providers in config.yml

| Provider | Required fields | Optional fields |
|---|---|---|
| `lm_studio` | `model` | `base_url` (default: `http://localhost:1234/v1`) |
| `ollama` | `model` | `base_url` (default: `http://localhost:11434`) |
| `openai` | `model`, `api_key` | `base_url`, `strength`, `scope` |
| `anthropic` | `model`, `api_key` | `strength`, `scope` |
| `open_ai` (OpenRouter) | `model`, `api_key`, `base_url` | `strength`, `scope`, `extra_params` |

`extra_params` is merged into the request body top-level (used for provider-specific fields like `reasoning.effort`).

---

## config.yml vs DB

| What | Where | How to change |
|---|---|---|
| Server host/port | `config.yml` | Edit file, restart app |
| DB path | `config.yml` | Edit file, restart app |
| History/round limits | `config.yml` | Edit file, restart app |
| LLM providers | DB (`llm_providers`) | UI or REST API |
| LLM models | DB (`llm_models`) | UI or REST API |
| MCP servers | DB (`mcp_servers`) | `register_mcp` tool or UI |
| Cron jobs | DB (`scheduled_jobs`) | `execute_task` tool (mode=cron) or UI |

---

## When to Update This File

- `Config` struct gains or loses a field
- Log level semantics change (see also `memory/feedback_logging.md`)
- `config.yml` gains a new section or key
