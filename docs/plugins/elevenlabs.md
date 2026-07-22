# ElevenLabs

- **Plugin id:** `elevenlabs`
- **Category:** Cloud text-to-speech + transcription
- **Runs:** calls the ElevenLabs cloud API — needs internet and an ElevenLabs account

## What it does

Adds [ElevenLabs](https://elevenlabs.io) as a provider of both realistic text-to-speech voices and speech-to-text transcription. Unlike the local TTS/transcription plugins, nothing runs on this machine — every request goes out to ElevenLabs' servers and is billed on their account.

Enabling the plugin does not by itself add any voice or transcription model — it only makes "ElevenLabs" available as a provider type. Models are added separately in the Models hub (see below), same as any other cloud LLM/TTS/transcription provider in this app.

## Requirements

- An ElevenLabs account and an API key (from the ElevenLabs dashboard).

## Enabling & configuring (admin)

1. Plugin catalog → **ElevenLabs** → enable. (This plugin has no config form of its own.)
2. Go to the Models hub → **LLM Providers**, add a new provider, choose type **ElevenLabs**, paste the API key (stored as a secret field, not shown again after saving).
3. Go to the Models hub → **Transcription** and/or **TTS**, add a model, pick the ElevenLabs provider just created, and choose a voice/model from the list — fetched live from ElevenLabs, so it always reflects what's actually available on that account.

## Notes

- Usage is billed by ElevenLabs per character (TTS) / per minute (transcription) according to the account's plan.
- Several ElevenLabs voice models understand inline markup for expressiveness: `<break time="0.5s" />` for pauses, `<phoneme alphabet="ipa" ph="...">word</phoneme>` for pronunciation, `(laughs)`/`(sighs)`/`(coughs)` for non-verbal sounds, and ALL CAPS or repeated letters for emphasis. Worth mentioning to a user who wants more expressive speech.
- One provider entry can supply both a TTS model and a transcription model — they don't need to be added twice.
