# Plugin System

Plugins are long-running subsystems compiled into the binary and managed by `PluginManager`. They receive a `PluginContext` (a bundle of `Arc<dyn Trait>` deps) and run independently from the Axum web server.

---

## Plugin Trait

```rust
#[async_trait]
pub trait Plugin: Send + Sync {
    fn id(&self)          -> &str;
    fn name(&self)        -> &str;
    fn description(&self) -> &str;
    fn is_running(&self)  -> bool;
    async fn start(&self, ctx: PluginContext)  -> Result<()>;
    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()>;
    async fn stop(&self)  -> Result<()>;
    /// Optional: contribute one Axum router nested under `/api/plugin/<id>/`.
    fn http_router(&self) -> Option<axum::Router> { None }
    fn as_any(&self)      -> &dyn Any;
}
```

`start()` and `reload()` must **spawn internal tasks and return immediately**.
`stop()` must cancel all internal tasks and **await their completion**.

`PluginContext` (`crates/core-api/src/plugin.rs`) carries typed `Arc<dyn Trait>` deps sourced from `Skald` — plugins use only what they need. Beyond the registry/provider deps, it includes:

- `db: Arc<sqlx::SqlitePool>` — Skald's shared SQLite pool (create/use plugin-owned tables here).
- `inbox: Arc<dyn InboxApi>` — unified approvals + clarifications façade (`list_pending`, `approve`, `reject`, `answer`); idempotent by `request_id`.
- `chat_hub.events(...)` — subscribe to the global `GlobalEvent` bus (including the four Inbox events; see [approval/index.md](approval/index.md)).

### Plugin HTTP routes (`http_router`)

A plugin can mount HTTP routes by overriding `http_router()` (default `None`). After `start_enabled()`, `WebFrontend::start` collects routers via `PluginManager::collect_plugin_routers()` and nests each enabled plugin's router under `/api/plugin/<id>/`, behind Skald's normal auth. The router must close over the plugin's own state (no axum `State` is passed). Only routes — no nav entries / JS assets / Lit components. The mesh-facing router (`router_factory`) does not include plugin routes.

---

## PluginManager — `src/core/plugin/mod.rs`

| Method | Purpose |
|---|---|
| `register(plugin)` | Add a plugin before startup (build phase) |
| `set_skald(Arc<Skald>)` | Wire core after `Skald` is built |
| `set_router_factory(factory)` | Provide Axum router factory (called by `WebFrontend`) |
| `set_web_port(port)` | Provide HTTP port for plugins that need it (called by `WebFrontend`) |
| `start_enabled()` | Start all DB-enabled plugins |
| `collect_plugin_routers()` | Gather `(id, Router)` of enabled plugins to nest under `/api/plugin/<id>/` (called by `WebFrontend` after `start_enabled`) |
| `stop_all()` | Graceful shutdown on SIGINT |
| `toggle(id, enabled)` | Enable/disable and start/stop at runtime |
| `list()` | `Vec<PluginInfo>` with enabled + running flags |

### Startup sequence

```
main.rs:
  plugins = vec![Arc::new(HonchoPlugin::new()), Arc::new(TelegramPlugin::new(...)), …]

Skald::new(core_cfg, plugins):
  PluginManager::new(pool) → register_arc(plugin) for each plugin → Arc::new(plugin_manager)
  → build tool_registry (list_items / toggle_item reference Arc<PluginManager>)
  → Arc::new(Skald { … })
  → plugin_manager.set_skald(Arc::clone(&skald))

WebFrontend::start():
  → plugin_manager.set_router_factory(factory)
  → plugin_manager.set_web_port(port)
  → plugin_manager.start_enabled().await
  → plugin_manager.collect_plugin_routers().await   // nest under /api/plugin/<id>/
```

### Enabled/disabled persistence

Plugin state and configuration are stored exclusively in the `plugins` SQLite table (`id TEXT PK, enabled INTEGER, config TEXT`). `config.yml` has no plugin section — plugins are configured at runtime via the REST API (`PUT /api/plugins/{id}`) or by asking the agent to use `toggle_item` (kind=plugin).

---

## Adding a New Plugin

Plugins live in independent workspace crates (see [crates/workspace.md](crates/workspace.md)):

1. Create `crates/plugin-<name>/` with a `Cargo.toml` depending on `core-api` and any needed external crates.
2. Implement `core_api::plugin::Plugin` in `crates/plugin-<name>/src/lib.rs`.
3. Add the crate to the workspace `members` list and as a dependency in the main `Cargo.toml`.
4. In `src/main.rs`, add `Arc::new(plugin_name::MyPlugin::new(...))` to the `plugins` vec before `Skald::new()`.
5. Rebuild — no restart needed for toggle.

---

## LLM Tools

| Tool | Description |
|---|---|
| `list_items` (type=plugins) | All plugins with enabled + running status |
| `toggle_item` (kind=plugin) | Enable/disable a plugin by id (immediate + persisted) |
| `configure_plugin` | Update a plugin's config JSON and restart it immediately |

### Plugin-provided global LLM tools

The `Plugin` trait has **no** `tools()` method: it cannot register global LLM tools by itself. The pattern (used by `mobile-connector`) is:

1. The plugin exposes a domain control trait (e.g. `RelayAgent`) and implements it on the plugin struct.
2. The plugin crate exports a factory `fn xxx_tools(agent: Arc<dyn Trait>) -> Vec<Arc<dyn Tool>>`.
3. In `Tools::build` (`src/core/skald/bundles.rs`), while the registry is being assembled, the plugin is fetched via `PluginManager::get_plugin_typed::<P>()`, cast to the trait, and its tools are registered with `ToolRegistry::register_arc(...)`.

The tools call into the plugin lazily, so they can be registered before the plugin's runloop starts (they return an error while the plugin is stopped). This keeps the `core-api` `Plugin` trait minimal while still allowing tool-based control surfaces (plugin.md §11).

---

## Available Plugins

| Plugin id | Source | Doc |
|---|---|---|
| `honcho` | `crates/plugin-honcho/src/lib.rs` | [honcho.md](honcho.md) |
| `remote_connectivity` | `crates/plugin-tailscale-remote/src/lib.rs` | [remote.md](remote.md) |
| `telegram` | `crates/plugin-telegram-bot/src/lib.rs` | [plugins/telegram.md](plugins/telegram.md) |
| `whisper_local` | `crates/plugin-transcribe-whisper-local/src/lib.rs` | [whisper-local.md](whisper-local.md) |
| `comfyui` | `crates/plugin-comfyui/src/lib.rs` | [providers/image.md](providers/image.md) |
| `mobile-connector` | `crates/plugin-mobile-connector/src/lib.rs` | [mobile-connector.md](mobile-connector.md) |

---

## Transcribe Providers and TranscribeManager

Speech-to-Text is decoupled from the plugin system via `TranscribeManager` (`src/core/transcribe/mod.rs`).

Plugins that provide transcription (e.g. `whisper_local`) register an `Arc<dyn Transcribe>` in `skald.transcribe_manager` at `start()` and deregister at `stop()`. Non-plugin providers (e.g. a future OpenRouter client) can register directly at startup without needing a full plugin lifecycle.

```rust
// trait — src/core/transcribe/mod.rs
pub trait Transcribe: Send + Sync {
    fn id(&self) -> &str;
    async fn transcribe(&self, audio: Vec<u8>, format: &str) -> Result<String>;
}
```

| Method | Purpose |
|---|---|
| `register(Arc<dyn Transcribe>)` | Add/replace a provider by id |
| `unregister(id)` | Remove a provider |
| `get()` | Returns the first available provider |

Selection strategy is currently **first registered**. Callers (e.g. Telegram) ask `skald.transcribe_manager.get().await` — they never reference a concrete type.

See [whisper-local.md](whisper-local.md) for the only current provider.

---

## Image Generators and ImageGeneratorManager

Image generation is decoupled from the plugin system via `ImageGeneratorManager` (`src/core/image_generate/`) and two traits in `core-api::image_generate` — same split as `TranscribeProvider` / `TranscribeRegistry`.

Two kinds of providers coexist:

| Kind | Source | Example |
| --- | --- | --- |
| **DB-backed** | `image_generate_models` table, built from `llm_providers` credentials | OpenRouter `x-ai/grok-2-vision` |
| **Plugin-registered** | Ephemeral — registered at runtime by plugins | ComfyUI workflows |

Plugins register via `ctx.image_generate_registry` (type `Arc<dyn ImageGenerateRegistry>`) in `PluginContext`. No dependency on the main crate is needed — only `core-api`.

```rust
// crates/core-api/src/image_generate.rs
pub trait ImageGenerate: Send + Sync { fn id(&self) -> &str; fn name(&self) -> &str; async fn generate(&self, prompt: &str) -> Result<Vec<u8>>; }
pub trait ImageGenerateRegistry: Send + Sync { async fn register(&self, provider: Arc<dyn ImageGenerate>); async fn unregister(&self, id: &str); }
```

| Method | Purpose |
|---|---|
| `ctx.image_generate_registry.register(...)` | Add a plugin provider (ephemeral) |
| `ctx.image_generate_registry.unregister(id)` | Remove a plugin provider |
| `add_model / update_model / delete_model` | DB-backed CRUD (called by REST API) |
| `list()` | Returns all active providers (plugin + DB) — used by LLM tool |
| `get(id)` | Returns a specific provider by id |
| `generate(provider_id, prompt)` | Generates and saves image, returns `(PathBuf, url)` |

The LLM interacts with providers via two tools: `image_generate_providers_list` and `image_generate`. See [providers/image.md](providers/image.md) for the full flow.

---

## TTS and TtsManager

Text-to-speech follows the same split pattern as transcribe and image_generate. `TtsManager` (`src/core/tts/`) manages both DB-backed and plugin-registered providers. Traits live in `core-api::tts`.

| Kind | Source | Example |
| --- | --- | --- |
| **DB-backed** | `tts_models` table, built from `llm_providers` credentials | OpenAI `tts-1-hd` |
| **Plugin-registered** | Ephemeral — registered at runtime by plugins | `OrpheusTtsPlugin` |

Plugins register via `ctx.tts_registry` (type `Arc<dyn TtsRegistry>`) in `PluginContext`.

```rust
// crates/core-api/src/tts.rs
pub trait TextToSpeech: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn description(&self) -> Option<&str>;
    fn instructions(&self) -> Option<&str>;  // default voice style
    async fn synthesize(&self, text: &str, instructions: Option<&str>) -> Result<Vec<u8>>;
}
pub trait TtsRegistry: Send + Sync {
    async fn register(&self, provider: Arc<dyn TextToSpeech>);
    async fn unregister(&self, id: &str);
}
```

| Method | Purpose |
|---|---|
| `ctx.tts_registry.register(...)` | Add a plugin TTS provider (ephemeral) |
| `ctx.tts_registry.unregister(id)` | Remove a plugin provider |

See [providers/tts.md](providers/tts.md) for the full manager API and DB schema.

---

## ApiProvider and ApiProviderRegistry

Plugins that supply an `ApiProvider` (e.g. a cloud TTS + Transcribe integration without subprocess) register via `ctx.api_provider_registry`:

```rust
// crates/core-api/src/provider.rs
#[async_trait]
pub trait ApiProvider: Send + Sync {
    fn type_id(&self)        -> &'static str;
    fn display_name(&self)   -> &'static str;
    fn supported_types(&self)-> &'static [ServiceType];
    async fn list_tts_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteTtsModelInfo>>>;
    async fn list_transcribe_models(&self, ..) -> ..;
    async fn list_llm_models(&self, ..)        -> ..;
    fn build_tts(&self, ..)             -> Option<Result<Arc<dyn TextToSpeech>>>;
    fn build_transcriber(&self, ..)     -> Option<Result<Arc<dyn Transcribe>>>;
    fn build_llm(&self, ..)             -> Option<Result<BuiltLlmClient>>;
    fn build_image_generator(&self, ..) -> Option<Result<Arc<dyn ImageGenerate>>>;
    fn ui_meta(&self) -> ProviderUiMeta;
}

pub trait ApiProviderRegistry: Send + Sync {
    fn register_plugin(&self, provider: Arc<dyn ApiProvider>);
    fn unregister_plugin(&self, type_id: &str);
}
```

`ProviderRegistry` in `src/core/provider/mod.rs` implements `ApiProviderRegistry`. It is exposed as `ctx.api_provider_registry` in `PluginContext`.

Plugin-registered providers shadow builtin ones: `ProviderRegistry::get(type_id)` checks the plugin list first.

**When to use `ApiProvider` vs `TtsRegistry`/`TranscribeRegistry`:**

- Use `TtsRegistry` / `TranscribeRegistry` for ephemeral local engines (subprocess, on-device model) that don't rely on the `llm_providers` credential DB.
- Use `ApiProvider` for cloud services that the user configures via the LLM-providers UI (API key stored in `llm_providers`, models managed via `tts_models` / `transcribe_models` CRUD).

All types used by `ApiProvider` (`LlmProviderRecord`, `LlmModelRecord`, `TtsModelRecord`, `TranscribeModelRecord`, `ImageGenerateModelRecord`, `RemoteLlmModelInfo`, `RemoteTtsModelInfo`, `RemoteTranscribeModelInfo`, `LlmStrength`) live in `core-api::provider` or their respective `core-api` modules. Plugin crates depend only on `core-api`.

---

## Plugin catalogue

| Plugin ID | Crate | Description |
|---|---|---|
| `honcho` | `crates/plugin-honcho` | Honcho long-term memory backend |
| `telegram_bot` | `crates/plugin-telegram-bot` | Private Telegram bot interface |
| `whisper_local` | `crates/plugin-transcribe-whisper-local` | Local STT via whisper.cpp |
| `tailscale_remote` | `crates/plugin-tailscale-remote` | Remote access via Tailscale mesh |
| `comfyui` | `crates/plugin-comfyui` | ComfyUI image generation workflows |
| `orpheus_tts_3b` | `crates/plugin-tts-orpheus-3b` | Local TTS via Orpheus 3B (Python subprocess) |
| `kokoro_tts` | `crates/plugin-tts-kokoro` | Local TTS via Kokoro ONNX (lightweight, multilingual) |
| `elevenlabs` | `crates/plugin-elevenlabs` | ElevenLabs cloud TTS and transcription (ApiProvider) |
| `mobile-connector` | `crates/plugin-mobile-connector` | Bridges the Inbox to mobile apps over the relay (E2E encrypted) — see [mobile-connector.md](mobile-connector.md) |
