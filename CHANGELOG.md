# Changelog

All notable changes to Skald Circle are recorded here, newest first.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions
are the workspace `Cargo.toml` version — the one `ci/verify-version.sh` checks before a
release PR may merge — and a section is closed at the commit that bumps it.

## [Unreleased]

### Added

- Several conversations per source: open extra chats with `+`, and the tab bar you left
  open is restored at your next login, on any device.
- A background task now reports back into the chat that started it instead of only the
  Inbox, and a chat shows the tasks still running under it.
- Skills reworked for the multi-user model: a shared tree plus a per-member one, with a
  generated index injected into the agent's prompt.
- The agent is told what its sandbox can actually run, from a probe of its own container.
- Event triage can be tuned per person: a check interval that overrides the instance one,
  and notification preferences read from `user-memory/notifications.md`.
- The assistant now remembers how you like emails and documents written — preferred
  wording, openings, sign-offs, formal vs. informal, per-recipient exceptions — as a short
  section of your private `user.md`, and applies it to later drafts.
- File viewer: syntax highlighting for code files and for code blocks in the chat, a
  hover copy button on those blocks, and history browsing for a file under git.
- Project explorer: download a folder as a streaming ZIP.
- Collapsible icon-only sidebar on desktop.
- DeepInfra, as a declarative LLM provider.
- The project coordinator offers to keep a history of a project.
- An agent can ask which connectors it holds instead of guessing.

### Changed

- Runtime image `v4`: Debian 13 base, plus the shared libraries a headless Chromium needs.
- Unencrypted users are unlocked and their runtimes started at boot, so Telegram, cron and
  the background agents work after a restart without anyone opening the web app first.
- PDFs render through pdf.js instead of an iframe.

### Fixed

- The server keeps running after you log out of the box; the install / update / uninstall
  scripts were hardened alongside it.
- Skald survives a restart of the Docker daemon.
- A user database gets the owner schema re-applied when it is opened.
- An approval bypass applies to the tool it was granted for, not to its whole connector.
- Connectors: an admin can use the ones they implicitly hold, per-user ones appear in the
  security-group picker, one whose process died is brought back, a global one's
  dependencies are installed where they are needed, and the prompt's connector list is
  rebuilt when the set changes.
- Telegram: pairing codes are no longer burned on the way out nor handed out unrecorded,
  and `send_attachment` resolves paths in the user's own workspace.
- The notification home is stored in the owner's database instead of the registry, where
  it silently dropped every batch it built.
- Event triage no longer notifies you *about* the messages your preferences told it to
  filter — a filtered event now produces silence rather than a notification explaining
  that it was filtered.
- LLM calls send the provider's model id on the wire rather than the local alias, and
  catalog capabilities resolve for reasoning-mode queries.
- `get_ast_outline` runs in the caller's workspace, gives a markdown heading a section
  range instead of a single line, and shows a proper name and icon on its chat card.
- The re-login dialog no longer hijacks the login screen, the new-chat `+` menu is visible
  and clickable, and the session-detail page stays live instead of freezing on a snapshot.
- A silently dead agent WebSocket is detected and redialled.
- A generated image lands in your own workspace instead of a server folder nobody could
  reach, so the assistant can finally send it to you on Telegram, open it in the viewer,
  or work on it with a command. It still shows inline in the web chat, its file is named
  after the prompt, and it is now readable only by the person who asked for it.

---

Releases up to and including `0.2.0` predate this file; `git log` is the record for them.
