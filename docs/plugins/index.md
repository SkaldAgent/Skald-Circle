# Plugin System

Extensible plugin architecture: lifecycle, trait contract, built-in plugins.

## Files

- [../plugins.md](../plugins.md) — Plugin trait, PluginManager, HTTP router integration
- **Built-in Plugins:**
  - [honcho.md](honcho.md) — Honcho long-term memory plugin: setup, config, filtering, lifecycle
  - [mobile-connector.md](mobile-connector.md) — Mobile app relay bridge, E2E encryption, Inbox sync (v2 protocol)
  - [telegram.md](telegram.md) — Telegram bot: setup, pairing, whitelist, HITL approval
  - [whisper-local.md](whisper-local.md) — Local STT via whisper.cpp (Metal-accelerated)
  - [remote.md](remote.md) — Tailscale mesh remote connectivity

See [../index.md#plugin-system](../index.md#plugin-system) for navigation.
