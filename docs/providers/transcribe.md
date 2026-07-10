# Transcription Providers

Cloud Speech-to-Text via any OpenAI-compatible audio transcription endpoint.

---

## Architecture

```text
crates/core-api/src/transcribe.rs
  — Transcribe trait (provider interface)
  — TranscribeProvider trait (resolve active provider)
  — TranscribeRegistry trait (plugin write-side: register/unregister)
  — TranscribeModelRecord     (DB record type — moved here from main crate)
  — RemoteTranscribeModelInfo (remote catalog type — moved here from main crate)

src/transcribe/
  mod.rs                — TranscribeModelInfo (API response type), re-exports from core-api
  db.rs                 — SQL layer for transcribe_models table
  manager.rs            — TranscribeManager (DB-aware, owns the table)
  openai_audio.rs       — OpenAiAudioTranscriber: impl Transcribe via HTTP multipart
  elevenlabs_audio.rs   — ElevenLabsTranscriber: impl Transcribe via ElevenLabs Scribe API
```

`TranscribeManager` holds two kinds of providers:

| Kind | Source | Example |
| ---- | ------ | ------- |
| **DB-backed** | `transcribe_models` table, built from `llm_providers` credentials | `OpenAiAudioTranscriber` |
| **Plugin-registered** | Ephemeral — registered at runtime by plugins | `WhisperLocalTranscriber` |

`get()` returns the first plugin provider (if any is running), then falls back to the first DB-backed provider ordered by `priority ASC`. Callers never reference a concrete type:

```rust
if let Some(t) = skald.transcribe_manager.get().await {
    let text = t.transcribe(audio, "ogg").await?;
}
```

### Manager API

```rust
// DB-backed CRUD — only TranscribeManager touches transcribe_models
transcribe_manager.add_model(record).await?
transcribe_manager.update_model(id, record).await?
transcribe_manager.delete_model(id).await?          // soft-delete
transcribe_manager.get_model(id).await              // → Option<TranscribeModelRecord>
transcribe_manager.list_models_info().await         // → Vec<TranscribeModelInfo> (DB-backed only)
transcribe_manager.list_all_info().await            // → Vec<TranscribeModelInfo> (plugins first, then DB — used by API)

// Remote model catalog (calls ApiProvider::list_transcribe_models)
transcribe_manager.list_provider_models(provider_id).await  // → Result<Vec<RemoteTranscribeModelInfo>>

// Plugin registration (ephemeral — called by WhisperLocalPlugin)
transcribe_manager.register(Arc::new(transcriber)).await
transcribe_manager.unregister("whisper_local").await
```

`RemoteTranscribeModelInfo` fields: `id`, `name`, `description`, `languages` (BCP-47 codes).

### REST API

| Method | Path | Description |
| ------ | ---- | ----------- |
| `GET` | `/api/transcribe/models` | List all models — plugin-registered first (`from_plugin: true`), then DB-backed |
| `POST` | `/api/transcribe/models` | Add a new transcription model |
| `GET` | `/api/transcribe/models/{id}` | Get a DB-backed model record |
| `PUT` | `/api/transcribe/models/{id}` | Update a DB-backed model |
| `DELETE` | `/api/transcribe/models/{id}` | Soft-delete a DB-backed model |
| `GET` | `/api/transcribe/providers/{id}/models` | List remote transcription models from a configured provider (`RemoteTranscribeModelInfo[]`) |

---

## OpenAiAudioTranscriber

Implemented in `src/core/transcribe/openai_audio.rs`.

Calls `POST {base_url}/audio/transcriptions` with a `multipart/form-data` body:

| Field      | Value                                            |
| ---------- | ------------------------------------------------ |
| `file`     | Raw audio bytes with extension-derived MIME type |
| `model`    | Provider model ID (e.g. `openai/whisper-1`)      |
| `language` | BCP-47 code (optional — omitted for auto-detect) |

Accepted formats: `ogg`, `mp3`, `mp4`, `m4a`, `wav`, `webm`, `flac`.
No local conversion needed — the provider handles decoding server-side.

### Supported providers

| Provider | `base_url` | Notes |
| -------- | ---------- | ----- |
| OpenRouter | `https://openrouter.ai/api/v1` | Model: `openai/whisper-1`, etc. |
| OpenAI | `https://api.openai.com/v1` | Model: `whisper-1` |

---

## ElevenLabsTranscriber

Implemented in `src/core/transcribe/elevenlabs_audio.rs`.

Calls `POST https://api.elevenlabs.io/v1/speech-to-text` with auth header `xi-api-key` (not Bearer) and a `multipart/form-data` body:

| Field | Value |
| ----- | ----- |
| `file` | Raw audio bytes |
| `model_id` | ElevenLabs Scribe model (e.g. `scribe_v1`) — stored as `model_id` in the DB record |

Returns `{ "text": "..." }`. Provider type: `elevenlabs`.

`ElevenLabsProvider::list_transcribe_models()` calls `GET https://api.elevenlabs.io/v1/models` and filters entries whose `model_id` starts with `scribe` (or `can_do_voice_conversion: true` as a fallback). Returns `RemoteTranscribeModelInfo` with id, name, description, and supported languages.

Which providers support transcription is declared statically via `ApiProvider::supported_types()` — see [llm-clients.md](llm-clients.md#apiprovider--service-types).

---

## DB: transcribe_models table

```sql
CREATE TABLE transcribe_models (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    provider_id INTEGER NOT NULL REFERENCES llm_providers(id),
    model_id    TEXT    NOT NULL,
    name        TEXT    NOT NULL UNIQUE,
    language    TEXT,                        -- BCP-47 or NULL for auto-detect
    priority    INTEGER NOT NULL DEFAULT 100,
    removed_at  TEXT,
    created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
    UNIQUE(provider_id, model_id)
)
```

`provider_id` references `llm_providers` — the same provider table used by LLM models.
Only providers that declare `ServiceType::Transcribe` in `supported_types()` should have rows here.

---

## DB insert — soft-delete revival

`transcribe_models` has two `UNIQUE` constraints: `name` and `(provider_id, model_id)`. `db::insert()` attempts to revive a soft-deleted row before falling back to a plain `INSERT` — same pattern as `tts/db.rs`. See [tts-providers.md](tts-providers.md#db-insert--soft-delete-revival) for the full description.

---

## When to Update This File

- A new concrete `Transcribe` implementation is added
- `transcribe_models` schema changes
- A provider gains or loses transcription support
