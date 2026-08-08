# Documentation index

This folder is written for **you, the assistant**, not for the human directly. It is mounted read-only at `~/docs/` in your workspace. Read it when a user asks how the software itself works, wants help configuring something, or asks what's possible — then explain it in your own words, adapted to that person (their technical level, their language, their actual goal). Don't just paste these files back at them.

Keep answers grounded in what's actually enabled and configured for this instance — check with the relevant tool (e.g. list installed/enabled plugins) rather than assuming everything described here is turned on. A feature documented here may not be enabled on this particular instance.

This index will grow over time. Right now it covers the interface, agents, memory, projects, shared folders, background tasks, system agents, access grants, connectors, skills, voice input and plugins; more sections (security groups…) will be added later.

## Features

| Document | What it covers |
| --- | --- |
| [memory.md](memory.md) | Private and shared memory: what goes where, the indexes and history log, why some shared facts can't be changed on request |
| [agents.md](agents.md) | Agents: the three kinds (chat, task, system), which one you are talking to and why, the specialist agents the assistant delegates to, how the model is chosen, and adding a custom agent |
| [projects.md](projects.md) | Projects: shared folders with their own assistant chat, a live file explorer, and member sharing |
| [shared-folders.md](shared-folders.md) | Shared folders: admin-managed folders with no chat of their own — who sees them, read vs write access, why the assistant asks before touching them, and when to choose a project instead |
| [system-agents.md](system-agents.md) | Background agents that run on a schedule (event triage, the two memory lints, the nightly conversation review of a supervised account): what they watch, why they only ever report, why a run can be skipped, and their settings |
| [tasks.md](tasks.md) | Background tasks: the strip above the message box, following one live, stopping one, answering the approvals and questions they raise, and how every outcome comes back to the conversation |
| [settings.md](settings.md) | The admin's Config page: interface language, the compaction model picker, debug mode |
| [access.md](access.md) | Who can use which plugin or connector: the open default, removing access per person, and the role switch that keeps children out of it |
| [connectors.md](connectors.md) | Connectors (MCP servers): shared vs per-user, setting one up in the UI, the sign-in and QR-pairing flows, and what to do when one is not working |
| [skills.md](skills.md) | Skills: instruction folders the assistant loads on demand — where they live, how to read and run one, and the contract for writing, installing and downloading one |
| [voice.md](voice.md) | Voice input: configuring a transcription model, and why the microphone button does nothing unless the page is served over HTTPS or localhost |
| [interface.md](interface.md) | The desktop interface: collapsing the sidebar to an icon-only strip to make room for documents |

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
- Enabling a plugin hands it to everyone straight away — except to roles that opt out of that (the Children role does). The admin then *removes* it from whoever should not have it, rather than granting it person by person. Full details in [access.md](access.md). (Mobile Connector is the one exception to the whole grant model: access there is the device-pairing itself, not a grant list.)
- Access is changed **per person, from that person's own page**: sidebar → Users → click the user → the **Plugins** section, right below their Connectors. So "what may this person use?" is answered in one place, for plugins and connectors together. (The plugin's own page shows the reverse view — who currently holds it — but read-only.) Admins can use every enabled plugin without being granted anything.
- A plugin with **per-user** settings (e.g. Telegram's pairing code, Honcho's memory opt-in) gives each granted user its own dedicated **sidebar page** to manage them — separate from the admin's instance-wide config.
- A plugin can add tools the assistant calls directly (e.g. `set_secret`, `telegram_pairing`), a dedicated sidebar page, or both.
