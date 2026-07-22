# Whisper Local

- **Plugin id:** `whisper_local`
- **Category:** Speech-to-text, local
- **Runs:** on this machine, in-process (via `whisper.cpp`, Metal-accelerated on Apple Silicon) — no cloud, no API key

## What it does

Transcribes voice messages entirely on-device using [whisper.cpp](https://github.com/ggerganov/whisper.cpp). Nothing leaves the machine — the right choice when privacy matters more than raw speed, or when there's no budget for a cloud transcription API.

The model (roughly 1–3 GB depending on size) is loaded into memory only when first needed ("lazy" loading) and unloaded again after a configurable idle period to free RAM — unless eager loading is turned on.

## Requirements

- A GGML `.bin` Whisper model file, downloaded manually onto the host filesystem — this plugin does not fetch it automatically. Example:
  ```
  curl -L -o models/ggml-large-v3.bin \
    https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin
  ```
  Smaller/faster models (e.g. `ggml-medium.bin`, `ggml-small.bin`) trade accuracy for speed and size — point at any of them.
- `ffmpeg` installed on the host (used to convert incoming audio to the 16 kHz mono format Whisper needs).

## Enabling & configuring (admin)

1. Download a model file first (see above) and note its path.
2. Plugin catalog → **Whisper Local** → enable, then **Configure**.
3. Fields:
   - **`model`** (required) — path to the `.bin` file, e.g. `models/ggml-large-v3.bin`.
   - **`language`** — a BCP-47 code (`it`, `en`, …) or `auto` for automatic detection (default `auto`).
   - **`load_at_startup`** — load the model into memory as soon as the plugin starts, instead of on first use (default off).
   - **`idle_timeout_secs`** — unload the model from memory after this many seconds of inactivity, `0` = never unload (default `1200`, 20 minutes).

## Notes

- The model occupies roughly 1–3 GB of RAM while loaded.
- The very first transcription after an idle unload is slower, since the model has to reload.
- No external service and no per-use cost — the trade-off is local CPU/GPU time and disk space for the model file.
