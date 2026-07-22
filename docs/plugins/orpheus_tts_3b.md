# Orpheus TTS 3B

- **Plugin id:** `orpheus_tts_3b`
- **Category:** Text-to-speech, local
- **Runs:** on this machine, as a Python subprocess this plugin manages — needs a real GPU for reasonable speed

## What it does

Expressive, high-quality local text-to-speech using the 3-billion-parameter Orpheus model. Heavier than Kokoro TTS, but supports inline emotion tags for much more natural-sounding speech: `<laugh>`, `<chuckle>`, `<sigh>`, `<cough>`, `<sniffle>`, `<groan>`, `<yawn>`, `<gasp>` — placed directly in the text where the effect should occur, e.g. *"He showed up at noon. \<chuckle\> Classic."*

The model is gated on HuggingFace, so it requires a personal access token before it can download.

## Requirements

- A HuggingFace account and access token: create one at <https://huggingface.co/settings/tokens>. It must be stored as a secret named `HUGGINGFACE_TOKEN` — this is **not** a plugin config field. If you (the assistant) are helping set this up, activate the config tools and call `set_secret("HUGGINGFACE_TOKEN", "hf_...")` with the token the user gives you; do not ask them to find a settings page for it. The plugin refuses to start without it.
- GPU VRAM, depending on the quantization level chosen: roughly 7 GB (`none`/fp16), 4 GB (`int8`, the default), or 2.5 GB (`int4`).
- `python3` on the host; the model itself downloads automatically from HuggingFace on first run and is cached locally.

## Enabling & configuring (admin)

1. Get a HuggingFace token and store it as the secret `HUGGINGFACE_TOKEN` (see above).
2. Plugin catalog → **Orpheus TTS 3B** → enable, then **Configure**.
3. Fields:
   - **`quantization`** (`none` | `int8` | `int4`, default `int8`) — lower precision uses less VRAM at some quality cost.
   - **`voice`** (`tara` | `dan` | `leah` | `zac` | `zoe` | `mia` | `julia` | `leo`, default `tara`).
4. No separate "add provider" step — it appears directly in the Models hub's TTS section once running.

## Notes

- Meaningfully heavier than Kokoro TTS: only worth choosing when the machine has a real GPU and the extra expressiveness (emotion tags) matters.
- If the plugin won't start, the most common cause is a missing or invalid `HUGGINGFACE_TOKEN` secret, or the model being gated and requiring the HuggingFace account to accept its license on the model page first.
