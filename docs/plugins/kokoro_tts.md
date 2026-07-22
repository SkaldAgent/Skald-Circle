# Kokoro TTS

- **Plugin id:** `kokoro_tts`
- **Category:** Text-to-speech, local
- **Runs:** on this machine, as a Python subprocess this plugin manages — CPU-only, no API key

## What it does

Lightweight, fast local text-to-speech using the Kokoro ONNX model. Runs on CPU (no GPU needed), supports several languages, and needs no account or API key. On first start it downloads its own model files (~310 MB + 27 MB) from GitHub releases and caches them locally — after that it works fully offline.

## Requirements

- `python3` available on the host (the plugin writes and spawns a small embedded Python server itself — nothing to install manually).
- Internet access the first time it starts, to download the model files.

## Enabling & configuring (admin)

1. Plugin catalog → **Kokoro TTS** → enable, then **Configure**.
2. Fields:
   - **`voice`** (default `if_sara`) — voice id. Prefix meaning: `a`=American, `b`=British, `i`=Italian, `j`=Japanese, `z`=Chinese; `f`=female, `m`=male. Includes `if_sara`, `im_nicola` (Italian), plus several English voices (`af_*`, `am_*`, `bf_*`, `bm_*`).
   - **`lang`** (default `it`) — language code for phonemisation: `it`, `en-us`, `en-gb`, `ja`, `zh`, `es`, `fr`, `hi`, `pt-br`, `ko`.
   - **`speed`** (default `1.0`, range `0.5`–`2.0`) — speech speed multiplier.
3. Changing the config restarts the underlying subprocess automatically. No separate "add provider" step — it appears directly in the Models hub's TTS section once running.

## Notes

- Best results when the text is written the way it would be spoken: short sentences, no markdown, no bullet points or symbols, standard orthography (accented letters are fine for Italian).
- Good default choice when a user wants spoken responses but has no GPU and no interest in paying for a cloud voice.
