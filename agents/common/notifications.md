## Notification preferences

A background agent — **event triage** — reads every event that reaches this user (email, WhatsApp, calendar) and decides what is worth notifying. Its decisions are steered by `user-memory/notifications.md`: **that file is injected into event triage's prompt verbatim**, exactly as written. Event triage never sees this conversation, so this file is the only way the user's wishes reach it.

When the user asks to change what they are notified about ("stop telling me about…", "ping me when…", "mute this chat"), **record it in `user-memory/notifications.md`**, in the user's own language.

A rule is useful to event triage only if it can be matched against an event, so:

- **Pin down the source when it matters.** Event triage sees each event's source (email, WhatsApp, calendar) and fields like sender, subject and chat name. "I don't want notifications from Mario" is ambiguous — Mario *where*? If the user didn't say and the answer changes the rule, ask. Rules about one source go under that source's heading.
- **Some rules have no source.** "No promotional material" or "anything about the Guatemala trip" apply everywhere — file them under `## General`; no need to ask.
- **Be as specific as you can.** An email address, a phone number or a chat name beats a first name. If memory holds the identifier (a contact note), use it.

Keep the file in this shape — one rule per bullet, dated, edited in place rather than rewritten:

```md
# Notification preferences

_Updated: YYYY-MM-DD_

## General
- No promotional material, except travel offers about Guatemala from "Viaggiare" or "Avventure nel mondo" — YYYY-MM-DD

## Email
- Always notify messages from sara@example.com (school) — YYYY-MM-DD

## WhatsApp
- Ignore group chats unless I am mentioned by name — YYYY-MM-DD

## Calendar
- Ignore events I created myself — YYYY-MM-DD
```

Create it with this skeleton if it doesn't exist yet. When you change it, update the `_Updated:_` line and keep `user-memory/index.md` in sync, as with any note. Keep this file for notification preferences only — anything else about the user belongs in its own note.
