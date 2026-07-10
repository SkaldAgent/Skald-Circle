# Custom slash commands

User-defined `/command` shortcuts that expand a **prompt template** into a normal
user message on the interactive session. They let you package a long, reusable
instruction behind a short command (`/deeploop`, `/review`, …) — the template is
interpolated with whatever the user typed and handed to the model as if the user had
written it out in full.

Because the expansion becomes an ordinary user turn on the `main` session, a command
is **fully interactive**: after it fires the model can ask follow-up questions,
iterate with the user, write files, and dispatch sub-agents — exactly like any other
turn. A command is not a one-off request.

## File layout (mirrors `agents/`)

Each command is a directory under `commands/`:

```
commands/<name>/meta.json     # manifest
commands/<name>/COMMAND.md     # the prompt template
```

- `<name>` is the directory name and the token typed after `/` (e.g. `commands/echo/`
  → `/echo`). Use lowercase `[a-z0-9_-]`.
- `meta.json`:
  - `description` (required) — one line, shown in `/help` and the composer autocomplete.
  - `enabled` (optional, default `true`) — set `false` to hide a command without deleting it.
- `COMMAND.md` — the template body. `{{args}}` (or `{{prompt}}`) is replaced with the
  user's arguments. If neither placeholder appears, the arguments are appended after a
  `---` separator (opencode-like default).

Files are read at **request time**, so edits to `meta.json` / `COMMAND.md` take effect
without a restart (same as `agents/*/AGENT.md`). There is no DB table and no CRUD API —
you create a command by adding files, just like an agent. A missing `commands/`
directory is fine (no custom commands).

## Precedence

Hard-coded **system** commands (`/help`, `/clear`, `/new`, `/model`, `/models`,
`/context`, `/cost`, `/compact`, `/resettools`, `/sethome`, `/stop`) always win: they
are matched first in each surface's command handler (the WS handler for web/mobile,
the Telegram plugin handler), so a custom command with a colliding name is
unreachable and is filtered out of `/help` and the autocomplete. An unrecognised `/…`
is rejected ("Unknown command") and never forwarded to the LLM.

## Dual view (typed command vs. expanded template)

The history row's `content` stores the **expanded template** (what the model replays);
the UI shows the **typed command** (`/echo ciao`), never the template. This is carried
by `MessageMetadata.command` (`CommandRef { name, display }`), persisted on the user
turn next to `attachments`:

- The backend substitutes `content = command.display` when emitting the `user_message`
  echo (`emitter.user_message`, both call sites in `llm_loop.rs` / `session/handler/mod.rs`)
  and when serialising history (`api/sessions.rs`).
- The frontend is **telnet-style**: custom commands are *not* echoed optimistically on
  send — the bubble appears only when the backend re-emits the `user_message` carrying
  the typed form. Only system commands (which reply with a `Done` and never echo back)
  are rendered optimistically. See `SYSTEM_SLASH_COMMANDS` in `web/lib/chat-session.js`.

## Composer autocomplete

Typing `/` in the copilot composer opens a dropdown: built-in system commands first,
then custom commands fetched from `GET /api/commands`. Filter by prefix, navigate with
`↑`/`↓`, accept with `Enter`/`Tab` (inserts `/name `), dismiss with `Esc`.
(`web/components/copilot.js`; styles in `web/css/copilot-input.css`.)

## Implementation map

| Concern | Location |
| --- | --- |
| Capability trait + DTOs + expansion | `crates/core-api/src/command.rs` (`CommandApi`, `CommandInfo`, `ResolvedCommand`, `expand_template`) |
| Discovery / resolve / expand | `src/core/command/mod.rs` (`LlmCommandManager: CommandApi`, owned by `Skald` as `Arc<…>`) |
| Metadata dual-view type | `crates/core-api/src/message_meta.rs` (`CommandRef`, `MessageMetadata.command`) |
| Matching + dynamic `/help` (web + mobile) | `src/frontend/api/ws.rs` (`dynamic_help`, custom-command interposition) |
| Matching + dynamic `/help` (Telegram) | `crates/plugin-telegram-bot/src/handlers.rs` (`help_text`, fallback-command arm; resolves via `ctx.command`) |
| Plugin wiring | `PluginContext.command` (`crates/core-api/src/plugin.rs`) → `TgShared.command` |
| Echo `content = display` | `src/core/session/handler/{llm_loop.rs, mod.rs}`, `src/frontend/api/sessions.rs` (applies to every surface: web, mobile, Telegram) |
| Listing endpoint | `GET /api/commands` → `src/frontend/api/commands.rs` |
| Autocomplete + telnet gate | `web/components/copilot.js`, `web/lib/chat-session.js` |

## Example

`commands/echo/meta.json`:

```json
{ "description": "Test command — echoes your input", "enabled": true }
```

`commands/echo/COMMAND.md`:

```
Repeat the user's input back verbatim.

User input:
{{args}}
```

Typing `/echo hello world` runs a turn where the model receives
`Repeat the user's input back verbatim.\n\nUser input:\nhello world`, while the chat
bubble shows `/echo hello world`.
