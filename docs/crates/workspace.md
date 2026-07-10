# Workspace Crates

Independent library crates in `crates/`. None depend on the main `skald` binary crate.

---

## `core-api` — `crates/core-api/`

Shared contract types and traits used by both the main crate and future independent plugin crates.

### Modules

| Module | Contents |
| --- | --- |
| `core_api::chatbot` | `ChatbotClient` trait, `Message`, `Role`, `ChatOptions`, `ChatResponse`, `LlmTurn`, `ToolCall`, `LlmRawMeta` |
| `core_api::provider` | `ApiProvider` trait, `ApiProviderRegistry` trait, `ProviderUiMeta`, `ProviderField`, `ServiceType`, `BuiltLlmClient`; DB record types: `LlmProviderRecord`, `LlmModelRecord`, `LlmStrength`, `RemoteLlmModelInfo` |
| `core_api::tts` | `TextToSpeech` trait, `TtsProvider`, `TtsRegistry`; `TtsModelRecord`, `RemoteTtsModelInfo` |
| `core_api::transcribe` | `Transcribe` trait, `TranscribeProvider`, `TranscribeRegistry`; `TranscribeModelRecord`, `RemoteTranscribeModelInfo` |
| `core_api::image_generate` | `ImageGenerate` trait, `ImageGenerateRegistry`; `ImageGenerateModelRecord` |
| `core_api::events` | `ServerEvent`, `GlobalEvent`, `ClientMessage`, `InboundDataMessage` |
| `core_api::bus` | `ChatEventBus` — in-process broadcast for completed turns |
| `core_api::interface_tool` | `InterfaceTool`, `ToolFuture` — LLM-callable tools injected by interfaces |
| `core_api::chat_hub` | `SendMessageOptions`, `ChatHubApi` trait |
| `core_api::location` | `GpsCoord`, `LocationEntry`, `LocationManager`; `LocationUpdater` trait |
| `core_api::tool` | `Tool` trait, `ToolCategory`, `ToolDescriptionLength`, `truncate_label` |
| `core_api::memory` | `Memory` trait — pluggable long-term memory backend contract |
| `core_api::remote` | `RemoteAccess` trait — mesh/remote-connectivity provider contract |
| `core_api::approval` | `ApprovalApi` trait — resolve pending tool-call approvals |
| `core_api::inbox` | `InboxApi` trait + `InboxSnapshot`/`InboxApprovalItem`/`InboxClarificationItem` — unified approvals + clarifications façade |
| `core_api::plugin` | `Plugin` trait, `PluginContext`, `RouterFactory` — plugin lifecycle contract and dependency bag |

### `PluginContext` fields (dependency bag)

`PluginContext` (`crates/core-api/src/plugin.rs`) carries the deps a plugin may need. Notable fields:

| Field | Type | Purpose |
| --- | --- | --- |
| `chat_hub` | `Arc<dyn ChatHubApi>` | Send messages; `events()` subscribes to the global `GlobalEvent` bus |
| `approval` | `Arc<dyn ApprovalApi>` | Resolve tool-call approvals |
| `inbox` | `Arc<dyn InboxApi>` | Unified approvals + clarifications: `list_pending`, `approve`, `reject`, `answer` (idempotent by `request_id`) |
| `db` | `Arc<sqlx::SqlitePool>` | Skald's shared SQLite pool — plugins create/use their own tables (e.g. `relay_*`) here |
| `event_bus` / `system_bus` | `Arc<ChatEventBus>` / `Arc<SystemEventBus>` | In-process buses |
| `web_port` / `remote_slot` / `router_factory` | — | Networking deps (mesh plugins) |

### Plugin HTTP routes (`Plugin::http_router`)

A plugin may contribute one `axum::Router` by overriding `fn http_router(&self) -> Option<axum::Router>` (default `None`, so existing plugins are unaffected). After `start_enabled()`, `WebFrontend::start` calls `PluginManager::collect_plugin_routers()` and nests each enabled plugin's router under `/api/plugin/<id>/`, behind Skald's normal auth. The router must close over the plugin's own state (it receives no axum `State`). Only routes are supported — no nav entries, JS assets, or Lit components (see plugin.md §12.3). The mesh-facing router built by `router_factory` does **not** include plugin routes.

### `ChatHubApi` trait

Defines the surface a plugin needs to interact with the agent system:

```rust
#[async_trait]
pub trait ChatHubApi: Send + Sync {
    async fn register(&self, source_id: &str);
    async fn send_message(&self, source_id: &str, prompt: &str, opts: SendMessageOptions) -> anyhow::Result<()>;
    async fn clear(&self, source_id: &str) -> anyhow::Result<i64>;
    fn events(&self, source_id: &str) -> broadcast::Receiver<GlobalEvent>;
    async fn set_home(&self, source_id: &str) -> anyhow::Result<()>;
    async fn context_info(&self, source_id: &str) -> anyhow::Result<(Option<i64>, Option<i64>)>;
    async fn force_compact(&self, source_id: &str) -> anyhow::Result<bool>;
    async fn resume(&self, source_id: &str) -> anyhow::Result<()>;
    async fn approve(&self, request_id: i64);
    async fn reject(&self, request_id: i64, note: String);
    async fn resolve_question(&self, source_id: &str, request_id: i64, answer: String);
}
```

`ChatHub` in `src/core/chat_hub/mod.rs` implements this trait. To call trait methods on `Arc<ChatHub>`, import the trait: `use crate::chat_hub::ChatHubApi as _;`.

### `InterfaceTool`

```rust
pub struct InterfaceTool {
    pub definition: Value,   // OpenAI tool definition
    pub handler: Arc<dyn Fn(Value) -> ToolFuture + Send + Sync>,
}
```

Interface tools are injected per-turn via `SendMessageOptions::interface_tools`. They are only visible to the root agent — sub-agents do not inherit them (except `activate_tools` which is re-injected explicitly).

---

## Plugin Extraction Roadmap

The goal is to allow plugins to live in their own workspace crates without depending on the full main binary. All plugins depend only on `core-api` and external crates.

### Extracted plugins

| Plugin | Crate | Doc |
| --- | --- | --- |
| `honcho` | `crates/plugin-honcho/` | [honcho.md](honcho.md) |
| `remote_connectivity` | `crates/plugin-tailscale-remote/` | [remote.md](remote.md) |
| `whisper_local` | `crates/plugin-transcribe-whisper-local/` | [whisper-local.md](whisper-local.md) |
| `telegram` | `crates/plugin-telegram-bot/` | [telegram.md](telegram.md) |
| `orpheus_tts_3b` | `crates/plugin-tts-orpheus-3b/` | [tts-providers.md](tts-providers.md) |
| `kokoro_tts` | `crates/plugin-tts-kokoro/` | [tts-providers.md](tts-providers.md) |
| `elevenlabs` | `crates/plugin-elevenlabs/` | [tts-providers.md](tts-providers.md) |
| `mobile-connector` | `crates/plugin-mobile-connector/` | [mobile-connector.md](mobile-connector.md) |

### Remaining in main crate

All plugins have been extracted to independent workspace crates. ElevenLabs (TTS + transcription) was extracted into `crates/plugin-elevenlabs/` — it registers itself as an `ApiProvider` so the existing `llm_providers` + `tts_models` / `transcribe_models` UI continues to work unchanged.

### All `core-api` contracts needed by plugins

| Dependency | Status |
| --- | --- |
| `core_api::chatbot::ChatbotClient` (+ associated types) | ✅ In `core-api` |
| `core_api::provider::{ApiProvider, ApiProviderRegistry, LlmProviderRecord, …}` | ✅ In `core-api` |
| `core_api::tts::{TextToSpeech, TtsProvider, TtsRegistry, TtsModelRecord, …}` | ✅ In `core-api` |
| `core_api::transcribe::{Transcribe, TranscribeProvider, TranscribeRegistry, TranscribeModelRecord, …}` | ✅ In `core-api` |
| `core_api::image_generate::{ImageGenerate, ImageGenerateRegistry, ImageGenerateModelRecord}` | ✅ In `core-api` |
| `core_api::events::{ServerEvent, GlobalEvent}` | ✅ In `core-api` |
| `core_api::interface_tool::InterfaceTool` | ✅ In `core-api` |
| `core_api::chat_hub::{ChatHubApi, SendMessageOptions}` | ✅ In `core-api` |
| `core_api::location::{GpsCoord, LocationManager, LocationUpdater}` | ✅ In `core-api` |
| `core_api::remote::RemoteAccess` | ✅ In `core-api` |
| `core_api::plugin::{Plugin, PluginContext, RouterFactory}` | ✅ In `core-api` |
| `core_api::bus::{BusEvent, ChatEvent, ChatEventRole, RecvError}` | ✅ In `core-api` |
| `core_api::memory::Memory` | ✅ In `core-api` |
| `core_api::tool::{Tool, ToolCategory}` | ✅ In `core-api` |
| `core_api::approval::ApprovalApi` | ✅ In `core-api` |
| `core_api::inbox::{InboxApi, InboxSnapshot, …}` | ✅ In `core-api` |

---

## Decoupling Pattern — OnceLock extraction

When a plugin cannot receive its typed deps at construction time (because `Skald` is built after plugin registration), use `std::sync::OnceLock` to extract and name the deps on first `start()`:

```rust
pub struct MyPlugin {
    // named, typed deps — no Arc<Skald>
    chat_hub:    OnceLock<Arc<dyn ChatHubApi>>,
    some_config: OnceLock<u16>,
}

fn extract_deps(&self, ctx: &PluginContext) {
    let _ = self.chat_hub.set(Arc::clone(&ctx.chat_hub));
    let _ = self.some_config.set(ctx.web_port);
}

async fn start(&self, ctx: PluginContext) -> Result<()> {
    self.extract_deps(&ctx);
    self.do_start().await  // no Skald needed here
}
```

`OnceLock::set` is idempotent — safe across multiple `reload()` calls. The values must be stable for the process lifetime (config values, `Arc` handles to singletons).

`RemotePlugin` (`crates/plugin-tailscale-remote/src/lib.rs`) uses this pattern with three deps: `port`, `remote_slot`, and `router_factory` — all sourced from `PluginContext`.

---

## `llm-client` — `crates/llm-client/`

Concrete LLM provider implementations: OpenAI-compatible, native Anthropic, Ollama, LmStudio.

Depends on `core-api` — `ChatbotClient` and all associated types (`Message`, `Role`, `ChatOptions`, `ChatResponse`, `LlmTurn`, `ToolCall`, `LlmRawMeta`) are defined there and re-exported from `llm-client` for backward compatibility.

Utility functions that depend on `reqwest` (`headers_to_json`, `redact_key`) remain in `llm-client` and are not part of `core-api`.

---

## `mcp-client` — `crates/mcp-client/`

MCP (Model Context Protocol) client over stdio and SSE transports. Used by `McpManager`.

---

## `honcho-client` — `crates/honcho-client/`

HTTP client for the Honcho long-term memory service. Used by `crates/plugin-honcho/`.

---

## `plugin-honcho` — `crates/plugin-honcho/`

Independent plugin crate for the Honcho long-term memory integration. Depends only on `core-api` and `honcho-client`. See [honcho.md](honcho.md).

---

## `plugin-tailscale-remote` — `crates/plugin-tailscale-remote/`

Independent plugin crate that exposes the web app on a Tailscale mesh network. Depends only on `core-api` and external crates (`tailscale`, `axum`, `tokio`, …). See [remote.md](remote.md).

Contains three modules:

| Module | Contents |
| --- | --- |
| `lib.rs` | `RemotePlugin` — plugin lifecycle, provider selection |
| `tailscale_sys.rs` | `TailscaleSystemProvider` — reads IP from system `tailscaled` daemon |
| `tailscale.rs` | `TailscaleEmbeddedProvider` — embedded netstack via `tailscale-rs` (feature-gated) |

Feature flags (in `crates/plugin-tailscale-remote/Cargo.toml`):

```toml
[features]
default = ["remote-tailscale"]
remote-tailscale = ["dep:tailscale"]
```

---

## `plugin-transcribe-whisper-local` — `crates/plugin-transcribe-whisper-local/`

Independent plugin crate providing local Speech-to-Text via whisper.cpp (Metal-accelerated on Apple Silicon). Depends only on `core-api`, `whisper-rs`, and `hound`. See [whisper-local.md](whisper-local.md).

`whisper-rs` and `hound` live exclusively in this crate — the main binary no longer depends on them directly.

### Key types

| Type | Role |
| --- | --- |
| `WhisperLocalPlugin` | `Plugin` impl — manages model lifecycle and registers/deregisters `WhisperLocalTranscriber` |
| `WhisperLocalTranscriber` | `Transcribe` impl — lightweight handle passed to `TranscribeManager` at `start()` |

Audio is converted to 16 kHz mono WAV via `ffmpeg` before being fed to whisper.cpp. Model must be a GGML `.bin` file; path is configured via the plugins REST API.

---

## `plugin-telegram-bot` — `crates/plugin-telegram-bot/`

Independent plugin crate for the private Telegram bot interface. Depends only on `core-api`, `teloxide`, and supporting crates (`tokio-util`, `chrono`, `rand`, `regex`). See [telegram.md](telegram.md).

`teloxide` and `tokio-util` live exclusively in this crate — the main binary no longer depends on them directly. The name `plugin-telegram-bot` distinguishes a bot-account integration from a potential future userbot (personal account) plugin.

### Source modules

| Module | Contents |
| --- | --- |
| `lib.rs` | `TelegramPlugin` — plugin lifecycle, bot startup, dispatcher wiring; `TgShared` holds `Arc<dyn TtsProvider>` |
| `events.rs` | `persistent_forwarder` — subscribes to ChatHub events and forwards to Telegram; `callback_handler` — inline keyboard button presses |
| `handlers.rs` | `message_handler`, `edited_message_handler` — incoming message classification and dispatch |
| `auth.rs` | `WhitelistFile`, pairing flow, `whitelist_watchdog` |
| `attachments.rs` | `TelegramAttachment` — download and describe documents, photos, locations |
| `helpers.rs` | `escape_html`, `label_to_html`, `send_long`, Markdown→HTML sanitizer |
| `tools.rs` | `interface_tools` (async) — `send_attachment` always present (sends images/videos inline by default, other types as a document, `as_document=true` to force a file); `send_voice_message` injected only when at least one TTS provider is active |

`send_voice_message` calls `TtsProvider::get()` at message time, synthesises text via the highest-priority active provider, and sends the result with `bot.send_voice()`. The tool's description automatically includes the provider's `instructions()` field so the LLM knows how to format text for that specific voice engine.

---

## `skald-relay-common` — `crates/skald-relay-common/`

Shared building blocks for the Skald Remote Control relay **and** the mobile-connector plugin, so both ends stay byte-identical on the wire and against the interop vectors (`data/ios-app/test-vectors.md`). Lightweight: **no** axum/tokio/Skald dependency.

Implements two transport versions:

| Module | Contents |
| --- | --- |
| `crypto` | Domain constants (`AUTH_DOMAIN`, `NS_DOMAIN`, KDF/session salts, direction prefixes), `decode_hex`, `namespace_id`, `challenge_message`, `sign_challenge`/`verify_challenge`, `ct_eq`; E2E suite: `derive_keys`, `ecdh`, `derive_aes_key`, `build_nonce`, `build_aad`, `seal`/`open` (AES-256-GCM) |
| `frames` | serde control-frame types for the **legacy v1** JSON wire protocol: `Incoming`/`Outgoing`, `AuthFrame`, `MessageIn`, `AuthorizeFrame`, `PairingStartFrame`, and the `codes` module (historical, no longer used by deployed code) |
| `proto` | **Active:** Protobuf-generated types for the **v2** binary wire protocol (`data/ios-app/v2/relay-protocol.md` §1-4). Compiled from `proto/skald/relay/v2/relay_frame.proto` by `build.rs` (prost-build). Exposed as `skald_relay_common::proto::v2` so future versions can sit alongside. `bytes` fields come through as `::prost::bytes::Bytes` (prost 0.13 default — wire-compatible with `Vec<u8>`). Every WebSocket binary frame post-auth is **exactly one** `RelayFrame` message. |
| `bin/gen-vectors` | Reference generator for the crypto interop vectors + protobuf encoding. Run with `cargo run -p skald-relay-common --bin gen-vectors`; a thin driver over the `crypto` library functions. Produces both v1 (JSON) and v2 (protobuf) test vectors. |

The relay only uses the verify/namespace subset of `crypto`; the full E2E suite is end-to-end between agent and client and used by the plugin + `gen-vectors`. See [plugin.md §1.1](../data/ios-app/plugin.md) and [relay.md §2](../data/ios-app/relay.md).

**Transport versions:**

- **v1** (JSON-text): frames in `frames` module; no app deployed
- **v2** (protobuf-binary): active; adds presence + live channel (route-or-fail). Documented in `data/ios-app/v2/` ([index.md](../data/ios-app/v2/index.md), [relay-protocol.md](../data/ios-app/v2/relay-protocol.md), [framing.md](../data/ios-app/v2/framing.md))

---

## `skald-relay-server` — `crates/skald-relay-server/`

Zero-trust store-and-forward relay + push bridge for the iOS/Android remote-control feature. Depends on `skald-relay-common` for **protobuf types** (`proto::v2`) and the verify/namespace crypto subset. Implements the **v2 binary wire protocol** (`data/ios-app/v2/relay-protocol.md`): every post-auth WebSocket binary frame is exactly one `RelayFrame` protobuf message. Includes presence tracking and a live channel (route-or-fail) for state pulls that don't queue. No `gen-vectors` binary here anymore (moved to the common crate).

### Source modules

| Module | Contents |
| --- | --- |
| `main.rs` | Thin binary entry: tracing init → `Config::from_env` → `AppState::build` → `axum::serve` with graceful shutdown |
| `lib.rs` | `AppState` (shared state, pusher wiring), `router`, `spawn_gc`, `shutdown_signal`. Conditionally swaps `LogPusher` for the live `ApnsPusher` when the `push-live` feature is on and APNs creds are present |
| `config.rs` | Env-driven `Config` (`bind`, `db_path`); `ApnsConfig` (team/key/PEM/bundle/sandbox) loaded from `APNS_KEY_PATH` + `APNS_BUNDLE_ID` + `APNS_SANDBOX` (gated by `push-live`) |
| `push.rs` | `Pusher` trait + `LogPusher` (default, redacted log only), `PushItem` + `apns_payload()`/`fcm_payload()` builders, the **content-in-push vs wake** decision (3500 B threshold, always compiled and unit-tested). Behind `push-live`: `ApnsPusher` (ES256 JWT provider auth, HTTP/2 over TLS via reqwest) and `build_pusher` factory |
| `store.rs` | `sqlx`/`sqlite` persistence: namespaces, clients, queue, pairing |
| `routing.rs` | `Registry` — in-memory `namespace_id → agent + clients` connection map; presence tracking per namespace |
| `auth.rs` | Re-export of `skald-relay-common::auth` (challenge verify, `namespace_id` derivation) |
| `types.rs` | Re-export of `skald-relay-common::proto::v2` (protobuf control-frame types for v2 binary transport) |
| `limits.rs` | Content-push byte cap, TTLs, rate-limits (`MAX_FRAME_BYTES` 64 KiB, `MAX_LIVE_FRAME_BYTES` 512 KiB per relay-protocol.md §5) |
| `ws.rs` | `handle_socket` — v2 transport driver (relay-protocol.md §1). Challenge → protobuf `Auth` decode → role dispatch → forward loop. Presence events, live (`Message.live=true`) route-or-fail dispatch, and store-and-forward queue. WS-level Ping/Pong keepalive (not protobuf messages). |

### Push bridge

The normative decision (content-in-push vs wake-only) and the JSON payload builders (`apns_payload` / `fcm_payload`) are always compiled and unit-tested — no Apple/Google credentials needed.

The **live senders** are behind the `push-live` cargo feature (default: off):

| Sender | Status | Notes |
| --- | --- | --- |
| `ApnsPusher` | ✅ implemented | ES256 JWT (refresh at 30 min, TTL 60 min) + HTTP/2 via reqwest ALPN. Headers: `apns-topic`, `apns-push-type` (`alert`/`background`), `apns-id` (UUID v4), `authorization: bearer <jwt>`. Body = `item.apns_payload()`. Sandbox vs prod selected by `ApnsConfig::sandbox`. Android notifications ignored (no FCM sender yet) |
| `FcmPusher` | ⬜ not implemented | FCM HTTP v1, OAuth2 service account, data-only message. Not in scope yet — until then, Android pushes are dropped at the platform check inside `ApnsPusher` |

Credentials: read at boot from `config/apns-key.json` (`{team_id, key_id, private_key}`) plus `APNS_BUNDLE_ID` / `APNS_SANDBOX` env vars. The PEM is already newline-decoded by `serde_json` and passed straight to `jsonwebtoken`. Logs never include the full `device_token` or any payload content — only `short()`-truncated identifiers and `apns-id` for correlation.

**Empty device tokens.** A client may connect (or pair) before its APNs/FCM registration has produced a token, sending an empty `device_token`. The relay treats an empty token as "none right now": `update_client_device_token` / `upsert_pending_client` keep any previously stored token instead of clobbering it (SQL `CASE WHEN ?n = '' THEN <existing> ELSE ?n END`), `forward_message` skips the push when the stored token is empty, and `ApnsPusher::notify` early-returns on an empty token. Without this, an empty token would build `/3/device/` and Apple returns `400 MissingDeviceToken`, silently breaking push routing for that device.

---

## `plugin-mobile-connector` — `crates/plugin-mobile-connector/`

The **agent** end of the relay protocol: bridges Skald's Inbox to mobile apps over a single permanent WebSocket, E2E encrypted. Depends on `skald-relay-common` (all of `crypto` + `proto::v2`) and `core-api` (`Plugin`, `InboxApi`, `ChatHubApi`, `SqlitePool`). Owns the `relay_clients` table and the `data/relay/seed` file. Implements the **v2 binary transport client** (`data/ios-app/v2/relay-protocol.md`): encodes/decodes `RelayFrame` protobuf, sends presence events, handles the live channel for `inbox_request` pulls. Implements `Plugin` (lifecycle + `http_router` for the QR endpoint) and `RelayAgent` (control surface). Exports `mobile_tools(agent)` so the main crate registers its three LLM tools. See [mobile-connector.md](mobile-connector.md).
