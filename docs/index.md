# Documentation index

This folder is written for **you, the assistant**, not for the human directly. It is mounted read-only at `~/docs/` in your workspace. Read it when a user asks how the software itself works, wants help configuring something, or asks what's possible — then explain it in your own words, adapted to that person (their technical level, their language, their actual goal). Don't just paste these files back at them.

Keep answers grounded in what's actually enabled and configured for this instance — check with the relevant tool (e.g. list installed/enabled plugins) rather than assuming everything described here is turned on. A feature documented here may not be enabled on this particular instance.

This index will grow over time. Right now it covers memory, projects, system agents and plugins; more sections (agents, connectors, security groups, shared folders…) will be added later.

## Features

| Document | What it covers |
| --- | --- |
| [memory.md](memory.md) | Private and shared memory: what goes where, the indexes and history log, why some shared facts can't be changed on request |
| [projects.md](projects.md) | Projects: shared folders with their own assistant chat, a live file explorer, and member sharing |
| [system-agents.md](system-agents.md) | Background agents that run on a schedule (event triage, the two memory lints): what they watch, why they only ever report, why a run can be skipped, and their settings |
| [settings.md](settings.md) | The admin's Config page: interface language, the compaction model picker, debug mode |

## Plugins

Plugins are optional add-ons an admin can enable and configure — extra voices, extra ways to reach the assistant (Telegram, a phone app), image generation, long-term memory, remote access, and so on. Each has its own document in [`plugins/`](plugins/):

| Document | What it adds |
| --- | --- |
| [plugins/comfyui.md](plugins/comfyui.md) | Local image generation via a self-hosted ComfyUI server |
| [plugins/elevenlabs.md](plugins/elevenlabs.md) | Cloud text-to-speech and transcription (ElevenLabs) |
| [plugins/whisper_local.md](plugins/whisper_local.md) | Local, private speech-to-text (no cloud, no API key) |
| [plugins/kokoro_tts.md](plugins/kokoro_tts.md) | Local, lightweight text-to-speech (CPU-only, no API key) |
| [plugins/orpheus_tts_3b.md](plugins/orpheus_tts_3b.md) | Local, expressive text-to-speech with emotion tags (needs a GPU) |
| [plugins/honcho.md](plugins/honcho.md) | Long-term cross-session memory via an external Honcho server (opt-in per user) |
| [plugins/telegram.md](plugins/telegram.md) | Chat with the assistant from Telegram |
| [plugins/mobile-connector.md](plugins/mobile-connector.md) | Companion mobile app: Inbox notifications + remote access, end-to-end encrypted |
| [plugins/remote_connectivity.md](plugins/remote_connectivity.md) | Reach the web app remotely over a Tailscale mesh network |

General plugin mechanics that apply to all of them:

- An admin enables/disables and configures each plugin from the **Plugins** page (sidebar → Plugins, admin-only): one card per plugin, an enable toggle, and a **Configure** button opening its settings form.
- A plugin only becomes visible to a given user once the admin grants them access — being enabled instance-wide isn't enough by itself (Mobile Connector is the one exception: access there is the device-pairing itself, not a grant list).
- A plugin with **per-user** settings (e.g. Telegram's pairing code, Honcho's memory opt-in) gives each granted user its own dedicated **sidebar page** to manage them — separate from the admin's instance-wide config.
- A plugin can add tools the assistant calls directly (e.g. `set_secret`, `telegram_pairing`), a dedicated sidebar page, or both.
