# Settings (Config page)

The **Config** page holds instance-wide settings. It is admin-only: what an admin changes here applies to every user of the instance.

Each setting is saved individually with its own **Save** button (a few, like the language, save as soon as they are changed).

## Interface

- **Language** — the default interface language for the whole instance. Each user can override it on their own profile page.

## TIC Agent

TIC is a background agent that watches events from connected MCP servers (new emails, calendar updates, WhatsApp messages…) and decides which ones are worth surfacing as notifications.

- **Enabled** — turn TIC on or off.
- **Security Group** — the tool permission group TIC's sessions run with; leave empty for the default group.
- **Check Interval (minutes)** — how often TIC runs; leave empty for the value from `config.yml`.

## Compaction

When a conversation grows too large, the oldest messages are automatically summarised so the context stays within limits. The summary replaces those messages in future turns while the most recent ones are kept verbatim.

- **Compaction model** — the model used to write those summaries, for the whole instance. Summarising is a simple writing task, so a cheap, fast model is usually the right choice — there is no reason to spend premium-model tokens on it. Leave it empty for automatic selection (by the `compaction.strength` value in `config.yml`, or the instance's default priority order). If the chosen model is later deleted, compaction silently falls back to automatic selection.

## Developer

- **Debug mode** — shows extra technical diagnostics in the interface.
