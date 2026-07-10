# LLM Clients

## Workspace Location

The `ChatbotClient` trait and all provider implementations live in the standalone crate `crates/llm-client` (no dependencies on the main crate). `src/core/chatbot/mod.rs` is a thin re-export layer. `LoggingChatbotClient` (`src/core/chatbot/logging.rs`) remains in the main crate because it depends on `sqlx`.

---

## ChatbotClient Trait

```rust
#[async_trait]
pub trait ChatbotClient: Send + Sync {
    async fn chat(&self, messages: &[Message], options: &ChatOptions) -> Result<ChatResponse>;
    async fn chat_with_tools(&self, messages: &[Value], tools: &[Value], options: &ChatOptions) -> Result<LlmTurn>;
    async fn chat_with_tools_raw(&self, messages: &[Value], tools: &[Value], options: &ChatOptions) -> Result<(LlmTurn, Option<LlmRawMeta>)>;
}
```

Only `AnthropicClient` and `OpenAiClient` implement native tool support (`chat_with_tools`). Other clients have a default fallback that strips tool definitions and calls `chat()` instead.

`chat_with_tools_raw` is used by the logging wrapper: it returns the same `LlmTurn` plus raw HTTP request/response metadata (`LlmRawMeta`). `AnthropicClient`, `OpenAiClient`, and `LmStudioClient` override it; all others fall back to calling `chat_with_tools` with no metadata.

`ChatOptions` carries two optional fields — `session_id` and `stack_id` — set by the LLM loop for correlation. Providers ignore them; only `LoggingChatbotClient` reads them.

---

## Transparent Request Logging

`LoggingChatbotClient` (`src/core/chatbot/logging.rs`) is a `ChatbotClient` wrapper that intercepts every `chat_with_tools` call:

1. Calls `inner.chat_with_tools_raw(...)` to capture the HTTP wire data.
2. Spawns a **fire-and-forget** `tokio::spawn` to insert a row into `llm_requests`.
3. Returns the `LlmTurn` to the caller unchanged.

The LLM loop is fully unaware — it holds an `Arc<dyn ChatbotClient>` and calls `chat_with_tools` as usual. The wrapper is applied in `LlmManager::build_entry` when `request_log_enabled = true` (set from `config.yml → llm.request_log.enabled`).

What is logged per row: full request body (provider-specific format), request headers (api-key redacted), full response body, response headers, token counts, round-trip duration, session/stack ID.

A background task (boot + every hour) deletes rows older than `retention_days` (default 14).

`LlmTurn` return variants:

- `Message(ChatResponse)` — final text answer
- `ToolCalls { content, calls, input_tokens, output_tokens, reasoning_content, cost }` — one or more tool calls requested

Both variants carry an optional `reasoning_content: Option<String>`. Populated only by providers that return chain-of-thought (currently DeepSeek thinking mode). Saved to `chat_history.reasoning_content` and echoed back on subsequent turns — see *Reasoning Content / DeepSeek Thinking Mode* below.

Both variants also carry an optional `cost: Option<f64>` — the request price in USD. Populated via the `ChatbotClient::extract_cost(&self, response: &Value)` trait method, whose default reads `usage.cost` from the raw JSON response (OpenRouter and other OpenAI-compatible gateways report it there). Providers that don't bill per-request leave it `None`. `llm_loop` persists it to `chat_history.cost`. Providers with a different response shape can override `extract_cost`.

---

## Provider Registry

Providers are no longer identified by a hard-coded enum. Instead, each provider is a struct implementing the `ApiProvider` trait (`src/core/provider/mod.rs`), registered at startup in `main.rs` via `ProviderRegistry::register_builtin()`. The DB column `llm_providers.type` stores the provider's `type_id` string.

```rust
// src/provider/mod.rs
#[async_trait]
pub trait ApiProvider: Send + Sync {
    fn type_id(&self) -> &'static str;         // e.g. "open_ai", "anthropic"
    fn display_name(&self) -> &'static str;
    fn supported_types(&self) -> &'static [ServiceType];

    // ── Remote model catalogs (default: Ok(None)) ─────────────────────────────
    async fn list_llm_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteLlmModelInfo>>>;
    async fn llm_model_info(&self, record: &LlmProviderRecord, model_id: &str) -> Result<Option<RemoteLlmModelInfo>>;
    async fn list_tts_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteTtsModelInfo>>>;
    async fn list_transcribe_models(&self, record: &LlmProviderRecord) -> Result<Option<Vec<RemoteTranscribeModelInfo>>>;

    // ── Factories (default: None) ─────────────────────────────────────────────
    fn build_llm(&self, record: &LlmProviderRecord, model: &LlmModelRecord) -> Option<Result<BuiltLlmClient>>;
    fn build_tts(&self, record: &LlmProviderRecord, model: &TtsModelRecord) -> Option<Result<Arc<dyn TextToSpeech>>>;
    fn build_transcriber(&self, record: &LlmProviderRecord, model: &TranscribeModelRecord) -> Option<Result<Arc<dyn Transcribe>>>;
    fn build_image_generator(&self, record: &LlmProviderRecord, model: &ImageGenerateModelRecord) -> Option<Result<Arc<dyn ImageGenerate>>>;
    fn ui_meta(&self) -> ProviderUiMeta;  // served to frontend via GET /api/llm/providers/types
}
```

`list_tts_models` and `list_transcribe_models` have a default implementation returning `Ok(None)` — providers that don't support listing do not need to implement them. Only `ElevenLabsProvider` currently overrides both, calling `GET https://api.elevenlabs.io/v1/models` and filtering by capability flag.

`BuiltLlmClient` bundles the constructed `Arc<dyn ChatbotClient>` with a `prompt_cache: bool` flag that controls whether Anthropic KV-cache headers are injected by the session loop.

**`ProviderRegistry`** (`src/core/provider/mod.rs`) holds built-in and plugin providers separately. Plugin providers shadow built-in ones with the same `type_id`. Plugins can call `registry.register_plugin()` / `registry.unregister_plugin()` at any time after startup.

`LlmManager`, `TranscribeManager`, `TtsManager`, and `ImageGeneratorManager` all receive an `Arc<ProviderRegistry>` at construction and use it to build clients and look up `supported_types`.

### Built-in Providers

| `type_id`     | Client struct     | api_key required | Default base_url                   | Prompt cache                   |
| ------------- | ----------------- | ---------------- | ---------------------------------- | ------------------------------ |
| `lm_studio`   | `LmStudioClient`  | No               | `http://localhost:1234/v1`         | ❌                             |
| `ollama`      | `OllamaClient`    | No               | `http://localhost:11434`           | ❌                             |
| `open_ai`     | `OpenAiClient`    | **Yes**          | `https://api.openai.com/v1`        | ❌                             |
| `openrouter`  | `OpenAiClient`    | **Yes**          | `https://openrouter.ai/api/v1`     | ✅ `anthropic/*` models only   |
| `anthropic`   | `AnthropicClient` | **Yes**          | `https://api.anthropic.com`        | ❌ (planned)                   |
| `deepseek`    | `OpenAiClient`    | **Yes**          | `https://api.deepseek.com/v1`      | ✅ automatic (see below)       |
| `zai`         | `OpenAiClient`    | **Yes**          | `https://api.z.ai/api/paas/v4`     | ❌                             |
| `elevenlabs`  | —                 | **Yes**          | `https://api.elevenlabs.io`        | ❌ (TTS + Transcribe only)     |

`openrouter`, `deepseek`, and `zai` reuse `OpenAiClient` with different base URLs. `elevenlabs` does not support LLM chat — `build_llm()` returns `None`.

`zai` (Z.AI / Zhipu AI GLM) has no `GET /models` endpoint, so `list_llm_models()` returns a **curated static catalog** of published GLM models (`glm-5.2`, `glm-5.1`, `glm-5`, `glm-5-turbo`, `glm-4.7`, `glm-4.6`, `glm-4.5`, `glm-4-32b-0414-128k`) with heuristic context lengths — the same UI "model picker" flow as `deepseek`/`openrouter`. Note the base URL ends in `/paas/v4` because `OpenAiClient` appends `/chat/completions`.

---

## Prompt Caching (KV Cache)

When `LlmEntry.prompt_cache = true` (currently set only for `OpenRouter`), the agent enables Anthropic-compatible KV caching on every request:

### What is sent

1. **`anthropic-beta: prompt-caching-2024-07-31` HTTP header** — tells OpenRouter/Anthropic to activate the caching feature.

2. **Static system message tagged for caching** — `build_openai_messages` emits the first system message (AGENT.md + memory files + `extra_system_static` + MCP list) as a content array with `cache_control: {"type": "ephemeral"}` on the single block. This is the KV cache prefix.

3. **Last tool tagged** — the final entry in the `tools` array receives `cache_control: {"type": "ephemeral"}`, caching the entire tool list as part of the prefix.

### Message order and cache stability

The full message array is structured so the stable prefix is as long as possible (see *Context Building* in [session.md](session.md)):

```text
[static system — cached]  [scratchpad?]  [summary?]  [conversation]  [dynamic tail]  [tail reminder]
```

The dynamic tail (Honcho memories + date/time) is placed **after** the conversation, so it never shortens the cacheable prefix. Scratchpad is a separate message so a mid-turn write only invalidates that small block, not the large static prefix.

### Cache TTL

Anthropic's `ephemeral` cache has a sliding TTL of ~5 minutes (extended by each hit). A cache hit is reported in the response as `cache_read_input_tokens` in the usage block.

### DeepSeek automatic KV cache

DeepSeek's disk KV cache is **prefix-based and fully automatic** — no explicit markers or special headers are required. Because the static system message is always the first entry in the message array, it becomes the stable cache prefix on every turn.

`prompt_cache = false` for the `DeepSeek` provider: no Anthropic-style `cache_control` blocks are injected (DeepSeek does not understand them). The cache operates transparently. DeepSeek reports cache hits in the response under `usage.prompt_cache_hit_tokens` / `usage.prompt_cache_miss_tokens` (visible in the raw request log).

#### ⚠️ The dynamic tail and cache invalidation

DeepSeek's KV cache compares requests **token by token from position 0**. If any token differs at position N, everything from N onward is recomputed — there is no partial matching inside the sequence.

The dynamic tail (date/time + Honcho memories) is injected as a system message at the end of the message array, after the conversation history. Because it is placed *after* the conversation, it doesn't shorten the cacheable prefix for the system message and tools. However, the **exact timestamp** (`2026-05-28T10:56:34+02:00`) changes every second. This means the stored KV entry from the previous request ends with `[..conversation..][dyn_tail_T1]`, while the new request has `[..conversation..][new_user_msg][dyn_tail_T2]`. The break point occurs right after the last assistant message: everything beyond it must be recomputed.

In practice this means: **without timestamp rounding, only the static system message and tools are effectively cached.** The conversation history accumulates in the cache prefix, but the always-changing timestamp prevents the prefix from extending into the tail message of the stored entry.

**Observed impact (production data):**

| Configuration | `prompt_cache_hit_tokens` | `prompt_cache_miss_tokens` |
| --- | --- | --- |
| Exact timestamp (default before fix) | ~6,144 | ~21,583 |
| `round_minutes: 15` | ~38,272 | ~830 |

With rounding, the timestamp string stays byte-identical for up to 15 minutes, letting the full conversation + tools accumulate in the cache prefix. The remaining ~830 miss tokens represent only the current user message (unavoidably new on every request).

### `llm.datetime` — timestamp injection config

Controlled by `config.yml → llm.datetime`:

```yaml
llm:
  datetime:
    enabled: true        # false = omit date/time from context entirely
    round_minutes: 15    # round down to nearest N-minute boundary
                         # e.g. 10:56 → 10:50 with round_minutes: 10
                         # omit for exact timestamp (hurts KV cache on prefix-based providers)
```

`round_minutes` is the primary tuning knob for cache efficiency on DeepSeek and any other prefix-based KV cache provider. The trade-off is precision: the LLM sees a timestamp that may be up to `round_minutes` minutes in the past. For most conversational uses this is imperceptible; for time-critical tasks (cron triggers, calendar scheduling) prefer a smaller value or `null`.

The default in `default.config.yaml` is `round_minutes: 15` — a safe value that gives near-100% cache hit rates in typical conversations while keeping the timestamp accurate to within a quarter-hour.

### Future: Anthropic direct

`AnthropicClient` does not yet support `prompt_cache`. The implementation is different: the `system` parameter must be sent as a JSON array of content blocks rather than a plain string. Tracked as a future improvement.

---

---

## Reasoning Content / DeepSeek Thinking Mode

When DeepSeek is configured with `"thinking": {"type": "enabled"}` in `extra_params`, each response includes a `reasoning_content` field alongside the normal `content`. This is the model's chain-of-thought.

**DeepSeek requires that `reasoning_content` be echoed back in the assistant message on subsequent turns.** Omitting it causes a `400 invalid_request_error`.

### How it works

1. `OpenAiClient.chat_with_tools_raw` reads `message.reasoning_content` from the response and propagates it through `LlmTurn`.
2. `llm_loop` saves it to `chat_history.reasoning_content` alongside the assistant's text content.
3. `build_openai_messages` includes `reasoning_content` in the reconstructed assistant message whenever the field is non-null.

All other providers always return `reasoning_content: None`; the field is simply absent from their assistant messages in the history.

---

## LlmStrength Enum

Ordered (weakest → strongest): `VeryLow` < `Low` < `Average` < `High` < `VeryHigh`

Used by AUTO selection and `call_agent` to match agents to capable models.

---

## AUTO Selection Algorithm

When `client = "auto"` or no client is specified, `LlmManager::select()` runs four passes in order, returning the first match:

1. Not-Down + strength ≥ required + scope matches
2. Not-Down + strength ≥ required (scope relaxed)
3. Any Not-Down model
4. **Emergency fallback**: strongest model even if Down (logs a `WARN`)

Models are ordered by `priority ASC` in the DB; lower number = tried first.

---

## Health Tracking

| Threshold                    | Status              |
| ---------------------------- | ------------------- |
| `consecutive_failures >= 3`  | `Degraded`          |
| `consecutive_failures >= 5`  | `Down`              |
| Next success                 | Reset to `Healthy`  |

`mark_failure()` is called by `run_agent_turn` on LLM call errors. `mark_success()` is called on every successful response. Health state is preserved across `reload()` calls (e.g. after adding a new model).

---

## Automatic LLM Failover

When the primary model returns a retriable error (5xx, network error, 429), `run_agent_turn` automatically tries the next available model — up to **3 attempts per round**.

**Retry logic**:

- A fresh `tried_this_round` list is built at the start of every round.
- On error, `is_retriable_llm_error()` decides whether to try again. Client errors (400/401/403/404/422) are **not** retried — the request itself is invalid.
- `select_excluding(&tried)` picks the next model, applying the same scope/strength rules as AUTO selection but skipping already-tried ones.
- If a different model uses different `prompt_cache` settings, messages are rebuilt before the retry.
- `cur_name`/`cur_llm` persist for the rest of the turn once switched, so subsequent rounds use the new model without re-trying the failed one.

**Events emitted**:

| Event | When | Who reacts |
| --- | --- | --- |
| `model_fallback` | Each successful switch | Frontend shows an inline info note |
| `llm_failed` | All attempts exhausted | Frontend shows error + `_waiting = false`; Telegram sends a message |

Telegram ignores `model_fallback` (silent retry) but sends an error message for `llm_failed`, matching the same behaviour as `Error`.

---

## Valid Scope Values

`basic`, `writing`, `coding`, `reasoning`, `math`, `search`

Defined by convention; any string is accepted by the DB. Agents declare `scope` in `meta.json`; models declare matching scopes in the DB.

---

## Extra Params

Each model can store an optional `extra_params` JSON object. Its top-level keys are **merged into the request body** before every API call, overriding any default key with the same name.

`OpenAiClient` (covers OpenAI-compatible providers) merges extra params via `apply_extra`. `AnthropicClient` merges them too via `apply_extra` (constructed with `with_extra_body`), which additionally enforces the extended-thinking constraints (see *Reasoning* below).

**Example — DeepSeek thinking mode (native DeepSeek provider):**

```json
{ "thinking": {"type": "enabled"}, "reasoning_effort": "high" }
```

`reasoning_effort` accepts `"low"`, `"medium"`, or `"high"`. Only supported by DeepSeek reasoning models (e.g. `deepseek-reasoner`); sending these params to non-reasoning models returns a 400.

**Example — DeepSeek reasoning effort on OpenRouter:**

```json
{ "reasoning": { "effort": "high" } }
```

Set via the model edit modal in the LLM Models UI, or via `PUT /api/llm/models/{id}` with `extra_params` in the JSON body.

> For the model's **reasoning / thinking** knob, prefer the dedicated `reasoning` field (below) over hand-writing provider-specific keys in `extra_params`.

---

## Reasoning

Reasoning ("thinking") is a first-class, provider-agnostic per-model setting rather than a hand-written `extra_params` blob. Two `ApiProvider` methods own it:

```rust
fn reasoning_mode(&self, model_id: &str, capabilities: &[String]) -> Option<ReasoningMode>;
fn reasoning_request(&self, value: &serde_json::Value) -> Option<serde_json::Value>;
```

- **`reasoning_mode`** describes the *control* (drives the UI). `ReasoningMode` is either:
  - `ValueSet { values, default }` — discrete choices (e.g. effort levels, or an on/off toggle), rendered as a dropdown.
  - `Range { min, max, step, default, unit }` — a numeric budget, rendered as a number input.
  - `None` — the model has no reasoning knob.
- **`reasoning_request`** translates the *selected value* (stored per model in `llm_models.reasoning`: a JSON string for a `ValueSet`, a JSON number for a `Range`, or `NULL` = off) into the provider-specific request fragment, merged into the request body at `build_llm` time via `providers::extra_with_reasoning`.

| Provider | `reasoning_mode` | Request fragment |
|---|---|---|
| `zai` | GLM-5.2+ → effort `ValueSet` (`disabled`/`minimal`…`max`); GLM-5.x/4.5/4.6/4.7 → on/off | `{"thinking":{"type":…}}` (+ `"reasoning_effort"` for GLM-5.2) |
| `deepseek` | reasoner / v4 → effort `ValueSet` (`disabled`/`low`…`max`) | `{"thinking":{"type":…}}` (+ `"reasoning_effort"`) |
| `openrouter` | per-model from the catalog `reasoning` object (`supported_efforts` / `supports_max_tokens`), else a fallback effort set | `{"reasoning":{"effort":…}}` or `{"reasoning":{"max_tokens":N}}` |
| `open_ai` | o-series / gpt-5 → effort `ValueSet` | `{"reasoning_effort":…}` |
| `anthropic` | thinking-capable models → `Range` (budget_tokens) | `{"thinking":{"type":"enabled","budget_tokens":N}}` |

The descriptor reaches the frontend two ways: attached to each catalog model (`RemoteLlmModelInfo.reasoning`) for the add-from-catalog form, on `LlmModelInfo.reasoning_mode` for the edit form, and via `GET /api/llm/providers/{id}/reasoning-mode?model_id=…` for the manual add form. The UI renders the matching control (dropdown / number input) with an "— off —" option (null value → no reasoning param sent).

**Anthropic** merges the fragment through `AnthropicClient::with_extra_body`; its `apply_extra` also enforces the extended-thinking constraints — `temperature` is dropped and `max_tokens` is bumped above `budget_tokens` when thinking is enabled.

---

## Model Metadata Fields

Each model record now stores additional metadata beyond the core LLM configuration:

| Field | Type | Source | Runtime use |
|---|---|---|---|
| `context_length` | `Option<i64>` | Provider catalog sync or manual input | Compaction threshold calculation, `max_tokens` limiting |
| `max_output_tokens` | `Option<i64>` | Provider catalog sync or manual input | Future: set `max_tokens` on LLM calls (currently `None`) |
| `knowledge_cutoff` | `Option<String>` | Provider catalog sync or manual input | Future: inject into system prompt |
| `capabilities` | `Vec<String>` | Provider catalog sync or manual input | Filtering by model feature (vision, function_calling, etc.) |

All fields are optional (`NULL` in the DB). When the provider catalog reports them,
they are automatically synced to existing DB records by `list_provider_models()`.
Manual values set via the API or UI take precedence when the provider does not
report a particular field (the sync uses `COALESCE` — only non-NULL catalog values
overwrite).

---

## LLM CRUD

All mutations go through `LlmManager` (not direct DB writes) because each operation calls `reload()` to rebuild the in-memory state:

- `add_provider()` / `update_provider()` / `delete_provider()`
- `add_model()` / `update_model()` / `delete_model()`

Setting `is_default = true` on a model automatically clears the flag on all others.

**Soft delete:** `delete_provider()` and `delete_model()` never issue `DELETE` statements. They set `removed_at = datetime('now')` on the row. Deleting a provider also cascades to all its models and clears the provider's `api_key`. Removed rows are excluded from `load_all_providers()` / `load_all_models()` and therefore from the in-memory state and AUTO selector. The `id` values remain valid as FK references in `chat_history.model_db_id`.

**Model identity is `name`.** `llm_models` has a single unique constraint, `name TEXT UNIQUE` — the alias, which is also the **resolution key** (`LlmManager` holds models in a `HashMap` keyed by name; `resolve(client_name)` / an agent's `client` look up by it). There is deliberately **no** `UNIQUE(provider_id, model_id)`: the same underlying model may be registered several times under one provider with different aliases and reasoning settings (e.g. `glm-4.6` vs `glm-4.6-thinking`). *(Migration v20 dropped the old `(provider_id, model_id)` constraint by rebuilding the table, preserving `id` values — they are FK targets in `llm_requests.model_db_id` / `chat_history.model_db_id`.)*

**Re-adding a removed model (revive):** rows are never hard-deleted, only soft-deleted (`removed_at`), and the `name` uniqueness ignores `removed_at`. So `insert_model()` upserts on `name`: `ON CONFLICT(name) DO UPDATE … removed_at = NULL`. Re-adding a model with the same alias **revives that row** (clearing `removed_at`, overwriting every field including `provider_id`/`model_id`/`reasoning`) instead of failing with a UNIQUE violation.

---

## ApiProvider — Service Types

Each provider declares which service kinds it supports via `ApiProvider::supported_types() -> &'static [ServiceType]`. Hardcoded per implementation — not stored in the DB.

`ServiceType` replaces the old `ModelType` enum (previously in `src/core/llm/providers/mod.rs`); it now lives in `src/core/provider/mod.rs` and is re-exported as `providers::ServiceType` for backwards compatibility.

| Provider (`type_id`) | `supported_types()`                              |
| -------------------- | ------------------------------------------------ |
| `openrouter`         | `[Llm, Transcribe, ImageGenerate, Tts]`          |
| `open_ai`            | `[Llm, Transcribe, Tts]`                         |
| `anthropic`          | `[Llm]`                                          |
| `ollama`             | `[Llm]`                                          |
| `lm_studio`          | `[Llm]`                                          |
| `deepseek`           | `[Llm]`                                          |
| `elevenlabs`         | `[Tts, Transcribe]`                              |

`supported_types` is included in the `GET /api/llm/providers` response so the frontend can filter provider dropdowns when adding TTS, transcription, LLM, or image generation models.

`GET /api/llm/providers/types` returns **all** registered provider types (no service-type filter). The frontend filters each picker independently using the `supported_types` array — e.g. the LLM model picker shows only providers where `supported_types.includes('llm')`.

---

## ApiProvider — Remote Model Catalog

`list_llm_models()` and `llm_model_info()` are methods on `ApiProvider`. They both receive the full `LlmProviderRecord` so they can read the `api_key` and `base_url` without constructing a separate credentials struct.

`RemoteLlmModelInfo` fields: `id`, `name`, `context_length`, `max_completion_tokens`,
`knowledge_cutoff`, `capabilities`, `vision: Option<bool>`, `price_input_per_million`, `price_output_per_million` (USD/M tokens).

`vision` is `Some(true/false)` when the provider reports it explicitly (e.g. OpenRouter `supported_parameters`), `None` when unknown.

| Provider (`type_id`) | `list_llm_models()` | `llm_model_info()` |
| --- | --- | --- |
| `openrouter` | `GET /api/v1/models` — sets `vision` from `supported_parameters` | — |
| `ollama` | `GET /api/tags` | `POST /api/show` |
| `anthropic` | `None` | `GET /v1/models/{id}` |
| `deepseek` | `GET /models` | `None` |
| `lm_studio` | `GET /v1/models` | `None` |
| `open_ai` | `None` | `None` |
| `elevenlabs` | `None` (LLM not supported) | `None` |

Provider instances are obtained via `ProviderRegistry::get(type_id)` — no on-demand factory needed.

### Model Catalog Cache

`LlmManager` caches `list_models()` results in memory, keyed by `provider_id`, with a **24-hour TTL**. The cache is discarded on process restart.

### Per-Model Metadata Cache

When `LlmManager::resolve()` is called, it lazily fetches `model_info()` for the resolved model
if the per-model cache is missing or older than **1 hour**. The `context_length` from the fresh
metadata is then propagated to the live `LlmEntry` in the model slot so subsequent turns use
the updated value.

Cache flow:

- Fast path: read lock on `model_meta_cache` → hit + fresh → return immediately.
- Miss / stale: fetch `model_info()` from the provider → update cache → update `LlmEntry.context_length`.
- Network failure: the old cached value (or DB value) is preserved — the error is silently ignored.

This ensures the compactor and any future `max_tokens` logic always have a reasonably current
`context_length` without blocking the first turn of the session.

```text
LlmManager::list_provider_models(provider_id)
  → cache hit  (< 24h old) → return cached Vec<RemoteLlmModelInfo>
  → cache miss / expired   → fetch via ApiProvider, store, return
```

API endpoint: `GET /api/llm/providers/{id}/models`

Used by the frontend "Add Model" wizard to populate the searchable model picker for OpenRouter, Ollama, and LM Studio providers.

---

## When to Update This File

- A new built-in provider is registered in `main.rs` (add row to the tables above)
- A new method is added to the `ApiProvider` trait
- The AUTO selection algorithm changes
- Health thresholds (`FAILURE_DEGRADED`, `FAILURE_DOWN`) change
- `ProviderRegistry` plugin API changes (register/unregister)
