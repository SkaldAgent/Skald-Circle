# WhisperLocal Plugin

Local Speech-to-Text via [whisper.cpp](https://github.com/ggerganov/whisper.cpp), Metal-accelerated on Apple Silicon.
Implemented in pure Rust using the `whisper-rs` crate — no Python involved.

---

## Setup

### 1. Download a GGML model

```sh
mkdir -p models
curl -L -o models/ggml-large-v3.bin \
  https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin
```

Other available sizes (smaller = faster, less accurate):

| Model | Size | Notes |
|---|---|---|
| `ggml-tiny.bin` | ~75 MB | Very fast, lower accuracy |
| `ggml-base.bin` | ~142 MB | Good balance for testing |
| `ggml-small.bin` | ~466 MB | Good accuracy |
| `ggml-medium.bin` | ~1.5 GB | High accuracy |
| `ggml-large-v3.bin` | ~3.1 GB | Best accuracy, recommended |
| `ggml-large-v3-turbo.bin` | ~1.6 GB | large-v3 speed-optimised |

All models: `https://huggingface.co/ggerganov/whisper.cpp`

### 2. Configure `config.yml`

```yaml
plugins:
  whisper_local:
    model: "models/ggml-large-v3.bin"
    language: "it"           # BCP-47 code, or "auto" for detection
    load_at_startup: false   # false (default) = lazy load on first use
    idle_timeout_secs: 1200  # unload after 20 min idle; 0 = never unload
```

| Option | Default | Effect |
|---|---|---|
| `model` | — (required) | Path to the GGML `.bin` file |
| `language` | `auto` | BCP-47 code or `auto`. Applied live — runtime changes take effect on the next transcription without a reload |
| `load_at_startup` | `false` | **When** the model first loads: `false` = lazily on the first transcription, `true` = eagerly in `start()` (warm, no first-call latency) |
| `idle_timeout_secs` | `1200` | **When** the model unloads: after this many seconds of inactivity. `0` = never unload (stays resident once loaded) |

The two timing options are orthogonal and cover the whole spectrum:

| `load_at_startup` | `idle_timeout_secs` | Behaviour |
|---|---|---|
| `false` | `1200` | **Default** — load on first use, free ~2 GB after 20 min idle |
| `true` | `0` | Always resident — eager load, never unload (legacy behaviour) |
| `true` | `1200` | Warm at startup, but freed if unused |
| `false` | `0` | Load on first use, then stay resident |

### 3. Build

The first `cargo build` compiles whisper.cpp (a few minutes). Subsequent builds are cached.

---

## How it works

```
Telegram voice message (OGG/Opus)
  │
  ▼ ffmpeg → 16 kHz mono WAV
  ▼ hound  → Vec<f32> PCM samples
  ▼ whisper.cpp (Metal GPU) → text
  │
  ▼ forwarded to LLM as a normal text message
```

Audio conversion uses the system `ffmpeg` binary (must be installed: `brew install ffmpeg`).
Inference runs on Apple Silicon GPU via Metal. Falls back to CPU if Metal is unavailable.

---

## Memory management (lazy load + idle unload)

The GGML weights are ~2 GB, so the plugin keeps them in memory only while they are
actually useful. The model lives in a shared, droppable cell (`LazyModel`):

- **`start()`** validates the model path and registers a lightweight transcriber, but
  does **not** load the weights unless `load_at_startup: true`. The registered handle
  holds no strong reference to the weights, so they can be freed at any time.
- **First transcription** triggers `ensure_loaded()`, which loads the weights once
  (concurrent first-callers wait on a single load) and records a last-used timestamp.
- A background **eviction task** ticks every 60 s and unloads the model once it has
  been idle for `idle_timeout_secs`. Set `idle_timeout_secs: 0` to disable eviction.
- **Unloading** is refcount-safe: an in-flight transcription holds its own handle to
  the weights, so memory is reclaimed only after it finishes. The actual free runs on
  a blocking thread (whisper.cpp GPU cleanup).

Trade-off: after an unload, the next transcription pays the reload cost (a few
seconds). The OS page cache usually keeps the `.bin` warm, so the reload is mostly
memory copy + Metal allocation rather than disk I/O. Use `load_at_startup: true` /
`idle_timeout_secs: 0` if you prefer zero first-call latency over reclaiming the RAM.

---

## Integration with TranscribeManager

`WhisperLocalPlugin` does **not** expose itself as `Arc<dyn Transcribe>` directly. At `start()` it registers a lightweight `WhisperLocalTranscriber` handle into `skald.transcribe_manager`; at `stop()` it deregisters it. Callers never reference the plugin type — they ask the manager:

```rust
if let Some(t) = skald.transcribe_manager.get().await {
    let text = t.transcribe(audio, "ogg").await?;
}
```

See [../plugins.md](../plugins.md) for the `TranscribeManager` API and the `Transcribe` trait.

---

## When to Update This File

- The audio conversion pipeline changes
- Default recommended models change
- Registration/deregistration logic in `start()`/`stop()` changes
- The lazy-load / idle-eviction lifecycle or its config options change
