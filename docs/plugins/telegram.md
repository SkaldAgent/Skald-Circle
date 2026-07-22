# Telegram Bot

- **Plugin id:** `telegram`
- **Category:** Messaging channel
- **Runs:** connects out to the Telegram Bot API — needs internet access

## What it does

Connects a private Telegram bot to this instance, so a user can chat with their assistant directly from Telegram — same assistant, same memory, same tools as the web app. Pending approvals (e.g. "can I run this command?") are shown as Telegram messages with inline buttons the user can tap to approve or deny, right from their phone.

One bot serves everyone on the instance; each person pairs their **own** Telegram chat to their **own** account — nobody sees anyone else's conversation.

## Requirements

- A Telegram bot token. Get one by messaging **@BotFather** on Telegram, sending `/newbot`, and following the prompts — it replies with a token that looks like `123456789:AAF...`.

## Enabling & configuring (admin)

1. Plugin catalog → **Telegram Bot** → enable, then **Configure**.
2. Field:
   - **`token`** (required) — the bot token from BotFather. Stored as a secret field, not shown again after saving.

## Per-user pairing (self-service)

Once the bot is enabled and a user has been granted access to the plugin:

1. The user opens Telegram, finds the bot (by the username chosen in BotFather), and sends it any message.
2. The bot replies with a short pairing code.
3. The user goes to their own Plugins page in the web app, finds Telegram, and pastes the code into the **pairing code** field.

That's the whole flow — no admin involvement needed for a normal pairing. (An admin *can* alternatively bind a chat to a user directly using the `telegram_pairing` tool from the assistant, e.g. if a user can't access the web app.)

## Notes

- Output sent to Telegram is automatically constrained to Telegram-safe HTML formatting (bold, italic, code blocks, links, quotes) — no Markdown, no tables. This is handled automatically; nothing to configure.
- Revoking a user's plugin access immediately stops that person's Telegram chat from working, without needing them to re-pair if access is restored later.
