# Telegram Plugin

A private Telegram bot that forwards messages to the LLM and supports Human-in-the-Loop approvals via inline keyboard buttons.

---

## Setup

1. Create a bot with [@BotFather](https://t.me/BotFather) and copy the token.
2. Add to `config.yml`:

```yaml
plugins:
  telegram:
    token: "123456789:AABBCC..."
```

3. Restart the app. The bot starts automatically if the token is present.

---

## Pairing — how to authorize a user

Access control is managed entirely through the file `secrets/telegram_whitelist.json`.

### File format

```json
{
  "whitelist": [123456789],
  "pending_pairings": [
    {
      "code": "A3KX7P",
      "chat_id": 987654321,
      "issued_at": "2026-05-19T10:30:00+02:00"
    }
  ]
}
```

- `whitelist` — array of authorized `chat_id` values (integers). Users in this array can send messages to the agent.
- `pending_pairings` — users who have contacted the bot but are not yet authorized. Each entry has a `code` shown to the user in Telegram chat, their `chat_id`, and the `issued_at` timestamp. Entries older than **24 hours** are pruned automatically the next time an unauthorized user contacts the bot, so abandoned codes do not pile up.

### Pairing flow

1. An unknown user sends any message to the bot.
2. The bot replies with a 6-character pairing code and writes an entry to `pending_pairings` in `secrets/telegram_whitelist.json`.
3. The user communicates the code to you (e.g., through a separate channel).
4. You ask the agent: *"Telegram pairing code A3KX7P — authorize it"*.
5. The agent reads `secrets/telegram_whitelist.json`, finds the entry with `code: "A3KX7P"`, moves the `chat_id` from `pending_pairings` to `whitelist`, and writes the file back.
6. Within 10 seconds the plugin's watchdog detects the file change, logs the event, and **sends a welcome message** to the newly authorized user on Telegram.

### To authorize manually (without asking the agent)

Use `edit_file` or `write_file` to move the `chat_id` from `pending_pairings` to `whitelist` in `secrets/telegram_whitelist.json`.

### To revoke access

Remove the `chat_id` from the `whitelist` array in `secrets/telegram_whitelist.json`. The change takes effect on the user's next message (whitelist is re-read on every message).

---

## Watchdog

The plugin polls `secrets/telegram_whitelist.json` every **10 seconds** for modification-time changes.

When it detects a change:
- Reloads the whitelist.
- Identifies any `chat_id` values newly added to `whitelist`.
- Sends each newly authorized user a welcome message on Telegram.
- Logs the event at INFO level.

This means there is no restart needed after editing the file — authorization takes effect automatically.

---

## Commands

| Command | Effect |
|---|---|---|
| `/new` or `/clear` | Create a new chat session (clears LLM context) |
| `/stop` | Interrupt the agent mid-turn (clears pending approvals and clarifications) |
| `/models` | List available LLM models ordered by priority (numbered `0..N`, index 0 is `auto`) |
| `/model <N\|name\|auto>` | Pin the model for this chat by index, name (substring allowed), or reset to `auto`. State is held in `ChatHub.selected_clients["telegram"]` and broadcast to all clients of the source via `ClientSelected`. Cleared on server restart |
| `/context` | Show last turn's token usage (`↑X tok · ↓Y tok`) |
| `/cost` | Show total spend for this session in USD (sync sub-agents included; async tasks excluded) |
| `/compact` | Force context compaction (bypasses the token threshold) |
| `/resettools` | Remove all activated tool groups (MCP servers + `config`) from the session |
| `/sethome` | Set Telegram as the home source for background notifications |
| `/help` | Show available commands |
| any other `/command` | Unknown — replies with a "Unknown command" notice + the help list, never forwarded to the LLM |
| any text | Forwarded to the LLM agent |

---

## Human-in-the-Loop Approvals

When the LLM triggers a tool that requires user approval (`execute_cmd`, `restart`, write-file tools outside `memory/`):

1. The bot sends a message with the operation details and a content preview.
2. Four inline keyboard buttons appear in two rows:

   ```text
   [✅ Approve]  [❌ Reject]
   [⏱ 15 min]   [🔄 Session]
   ```

3. Tapping a button resolves the pending approval and execution continues or is cancelled.
4. **⏱ 15 min** — approves and suppresses approval prompts for tools of the same category/MCP server for 15 minutes.
5. **🔄 Session** — approves and suppresses all approval prompts for the rest of the session.
6. The approval message is **deleted** once resolved, whether via Telegram or the web UI.

Bypass buttons call `ApprovalApi::approve_with_bypass` (scope auto-detected from the tool's category or MCP server). See [../approval/index.md](../approval/index.md) for bypass semantics.

---

## Output Formatting

Telegram's HTML parse mode supports only a limited tag set: `<b>` `<i>` `<u>` `<s>` `<code>` `<pre>` `<a>` `<blockquote>`. Structural elements (`<table>`, `<ul>`, `<li>`, `<div>`) are **not supported**.

The plugin injects a compact formatting context into every LLM session (`TELEGRAM_FORMAT_CONTEXT` in `mod.rs`) and a shorter tail reminder (`TELEGRAM_FORMAT_REMINDER`) instructing it to:

- Use Telegram HTML tags only.
- Never use Markdown (`**`, `*`, `` ` ``, `#`, `_`, `|`).
- Replace structured data (tables) with bullet lists (`•`).

| Element         | Correct               | Wrong              |
| --------------- | --------------------- | ------------------ |
| Bold            | `<b>text</b>`         | `**text**`         |
| Italic          | `<i>text</i>`         | `*text*`           |
| Code            | `<code>text</code>`   | `` `text` ``       |
| Code block      | `<pre>text</pre>`     | ` ```text``` `     |
| Structured data | bullet list `•`       | `\| col \| col \|` |

Long responses are automatically split into chunks of ≤ 4000 characters via `send_long()` in `helpers.rs`.

### Markdown sanitizer (post-processing safety net)

Because LLMs occasionally emit Markdown despite instructions, `send_long` applies `sanitize_for_telegram()` on every HTML-mode send **before** chunking. This provides a reliable fallback independent of model compliance:

1. **Markdown tables → bullet lists** — detects `| col | col |` blocks, emits the header row as `<b>header — header</b>` and each data row as `• val — val`.
2. **`**bold**` → `<b>bold</b>`** — converts residual Markdown bold.
3. **`## Header` → `<b>Header</b>`** — converts residual Markdown headers.

### Fallback behavior

If the Telegram API rejects a chunk (e.g., due to malformed HTML), `send_long` retries **without** `ParseMode::Html`. Before retrying it strips all HTML tags (`<…>`) from the chunk using a regex so the user sees plain text rather than raw `<b>…</b>` markup.

---

## Voice (Speech Integration)

If the Speech plugin is configured and running, the Telegram plugin gains two additional capabilities:

### Incoming voice messages (STT)

When the user sends a voice note, the plugin:

1. Downloads the OGG audio from Telegram.
2. Passes it to `SpeechPlugin::transcribe()`.
3. Forwards the resulting text to the LLM as a normal message.

### Outgoing voice replies (TTS)

The LLM has access to a `send_voice_message(text)` tool. When it calls it, the plugin:

1. Passes the text to the active `TextToSpeech` synthesiser (`synthesize()`).
2. **Transcodes the audio to Ogg/Opus** (`to_ogg_opus` in `tools.rs`) — the only format Telegram renders as a playable voice message. The synthesiser's `output_format()` decides the input: `opus`/`ogg` pass through untouched; raw `pcm` (e.g. Gemini TTS) is decoded as 24 kHz/mono/s16le; every other container (mp3, wav, …) is auto-detected. Conversion runs `ffmpeg` over stdin/stdout pipes (no temp files).
3. Sends the resulting Ogg/Opus bytes back to the user as a Telegram voice message.

The LLM is instructed to use voice only for short, conversational replies with no code or complex formatting. The TTS engine's formatting guide (SSML-like tags) is also injected into the system context so the LLM can control pacing and emphasis.

> **Requires `ffmpeg`** on `PATH` for any non-Opus synthesiser. If it is missing, `send_voice_message` returns a clear error (`ffmpeg not available …`) and the LLM falls back to a text reply.

### Requirements

Both `plugins.speech.stt_model` and `plugins.speech.tts_model` must be set in `config.yml`. The Speech plugin must be enabled and running before the Telegram plugin starts.

---

## File & Media Attachments

The Telegram plugin downloads incoming attachments and forwards them to the conversation. The LLM sees them in timeline order — it knows which file was most recently sent without any special indexing.

| Type | Saved to disk | How it reaches the LLM |
| --- | --- | --- |
| Document (PDF, ZIP, …) | `data/uploads/telegram/<chat_id>/<filename>` | Structured `metadata.attachments` (shared with web/mobile) |
| Photo | `data/uploads/telegram/<chat_id>/<file_id>.jpg` | Structured `metadata.attachments` |
| Location | — | `[TELEGRAM SYSTEM INFO]` text (latitude, longitude, accuracy, Google Maps URL) |

**Document and Photo are aligned with the web/mobile attachment model**: `download_and_save`
returns a `core_api::message_meta::Attachment` (project-root-relative path so `/data/…` serves
it), `handle_attachment` puts it in `SendMessageOptions.metadata`, and the message builder
generates the shared `[SYSTEM INFO]` block on the fly. Viewing the `telegram` source from the
copilot therefore shows these as **chips**, not raw text. See
[frontend.md](../frontend.md#attachments) and [database.md](../database.md) (`chat_history.metadata`).

**Location** has no file, so it keeps the legacy `[TELEGRAM SYSTEM INFO]` text path
(`system_info_message`). Captions typed alongside a Document/Photo become the user turn's text.

### Live locations

When the user shares a live location, two things happen:

1. **Initial message** (`message` event) — the LLM is notified via a `[TELEGRAM SYSTEM INFO]` message and the position is written to `skald.location_manager` under the key `"telegram"`.
2. **Subsequent updates** (`edited_message` events) — the position in `location_manager` is updated silently, with no LLM notification. This keeps the store current for any background scripts or tools that read `user_location("telegram")`.

`LocationManager` is in-memory only. On restart, the store starts empty and is repopulated as soon as Telegram delivers the next live location tick (typically within seconds if sharing is still active).

The `uploads/` directory is gitignored.

### Extending attachment types

To add a new type (e.g. sticker, contact):

1. Add a variant to `TelegramAttachment` in `crates/plugin-telegram-bot/src/attachments.rs`.
2. Implement `download_and_save`: return `Ok(Some(Attachment))` for a file-backed type (it flows into `metadata.attachments`) or `Ok(None)` for a file-less one (then add a `system_info_message` arm and handle it in the `None` branch of `handle_attachment`).
3. Detect the message type in `classify_message` in `handlers.rs` and return `IncomingEvent::Attachment(...)`.

---

## Interface Tools

The Telegram plugin can inject custom LLM-callable tools into any session via the `interface_tools` parameter of `SendMessageOptions`. These tools are only visible to the root agent — sub-agents do not inherit them.

To add a Telegram-specific tool, construct an `InterfaceTool` with an OpenAI tool definition and an async handler closure that captures `Arc<Bot>` and `ChatId`, then pass it in the `interface_tools` vec inside `SendMessageOptions`.

`InterfaceTool` and `ToolFuture` are defined in `crates/core-api/src/interface_tool.rs` (re-exported via `crate::chat_hub`). `AgentRunConfig` remains in `src/core/session/handler/interface_tools.rs` (main crate only).

---

## Secrets directory

`secrets/telegram_whitelist.json` is gitignored. The directory is created automatically on first pairing request. Never commit this file.
