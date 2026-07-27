# Settings (Config page)

The **Config** page holds instance-wide settings. It is admin-only: what an admin changes here applies to every user of the instance.

Each setting is saved individually with its own **Save** button (a few, like the language, save as soon as they are changed).

## Interface

- **Language** — the default interface language for the whole instance. Each user can override it on their own profile page.

## TIC Agent

TIC is a background agent that runs for each user in turn, reads the events that user's own connectors received (new emails, calendar updates, WhatsApp messages…) and decides which are worth surfacing as notifications to them. See [system-agents.md](system-agents.md) for how it works and why a user can be skipped.

- **Enabled** — turn TIC on or off for the whole instance, for everyone.
- **Security Group** — the tool permission group a TIC run uses. It is re-checked against each user's own role: if their role doesn't allow that group, their run falls back to the role's default. Leave empty to always use the role default.
- **Check Interval (minutes)** — how often a pass over all users starts; leave empty for the value from `config.yml`.

## Compaction

When a conversation grows too large, the oldest messages are automatically summarised so the context stays within limits. The summary replaces those messages in future turns while the most recent ones are kept verbatim.

- **Compaction model** — the model used to write those summaries, for the whole instance. Summarising is a simple writing task, so a cheap, fast model is usually the right choice — there is no reason to spend premium-model tokens on it. Leave it empty for automatic selection (by the `compaction.strength` value in `config.yml`, or the instance's default priority order). If the chosen model is later deleted, compaction silently falls back to automatic selection.

## Developer

- **Debug mode** — shows extra technical diagnostics in the interface.
