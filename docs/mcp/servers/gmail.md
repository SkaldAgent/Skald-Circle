# Gmail MCP Server (gmail)

## Overview

A Python MCP server providing **read, modify, and send** access to Gmail via the Gmail API v1.

**Server name:** `gmail`
**Transport:** `stdio` (spawns `python3 scripts/gmail_mcp_server.py`)
**Location:** `scripts/gmail_mcp_server.py`

The server also emits push notifications (`event/new_email`) to the agent when new mail lands in the INBOX (polled every 60 s via the Gmail History API).

### Permissions (safe by design)

| Capability | Yes/No |
|------------|--------|
| Read messages & threads | ✅ |
| Search messages | ✅ |
| List labels | ✅ |
| Modify labels (mark read, star, archive) | ✅ |
| Send email | ✅ |
| Create labels | ✅ |
| Download attachments | ✅ |
| Trash message (reversible) | ❌ *removed from tools* |
| Untrash message | ❌ *removed from tools* |
| Permanently delete | ❌ *scope not granted* |

The server uses the `gmail.modify` + `gmail.labels` scopes, which allow all operations **except permanent deletion**.

---

## Tools

All tools are callable as `mcp__gmail__<tool>`; the table lists the bare `<tool>` names.

| Tool | Required params | Optional params | Description |
|------|-----------------|-----------------|-------------|
| `status` | *(none)* | *(none)* | Self-check: verifies credentials load, the token refreshes, and the Gmail API responds. Call first when another gmail tool fails. |
| `list_messages` | *(none)* | `query`, `max_results`, `label_ids`, `page_token` | List messages with optional Gmail search query and label filter. Returns subject, sender, date, message/thread IDs. Pass `page_token` (from a previous response) to fetch the next page. |
| `get_message` | `message_id` | `include_body` | Read a single message by ID (body included by default, truncated at 10000 chars). HTML-only emails are converted to readable text; attachment filenames are listed. |
| `get_thread` | `thread_id` | *(none)* | Read all messages in a thread, newest last. |
| `list_labels` | *(none)* | *(none)* | List labels with total + unread counts (resolves label IDs). |
| `modify_message` | `message_id` | `add_labels`, `remove_labels` | Add/remove labels (mark read, archive, star). Each label arg accepts a string or array. |
| `send_message` | `to`, `subject`, `body` | `cc`, `bcc`, `in_reply_to`, `thread_id`, `attachments` | Send an email; supports in-thread replies and file attachments. |
| `get_profile` | *(none)* | *(none)* | Account email, total message/thread count, history ID. |
| `create_label` | `name` | `label_list_visibility`, `message_list_visibility` | Create a label; returns the new label ID. |
| `download_attachments` | `message_id` | `folder` | Save all attachments from a message. Defaults to `data/gmail_attachments/`. |

> **Removed:** `search_messages` (a literal alias of `list_messages` — use `list_messages` with the `query` parameter instead, which supports full Gmail search syntax).

### Gmail Search Syntax

The `query` parameter on `list_messages` supports Gmail's native syntax:

| Example | Meaning |
|---------|---------|
| `from:john@example.com` | Messages from a sender |
| `to:maria@example.com` | Messages to a recipient |
| `subject:meeting` | Messages with "meeting" in subject |
| `is:unread` / `is:starred` | Unread / starred messages |
| `in:inbox` / `in:sent` | Messages in Inbox / Sent |
| `after:2024/01/01` / `before:2024/06/01` | Date range |
| `has:attachment` | Messages with attachments |
| `label:MY_LABEL` | Messages with a custom label |

Combine with spaces: `from:john is:unread after:2024/06/01`.

---

## Authentication

### Credentials File

OAuth 2.0 user credentials are stored in:
- **Default path:** `./secrets/gmail_creds.json` (relative to project root)
- **Override:** `GMAIL_CREDS_PATH` env var

Required OAuth scopes (granted by the setup script):
```
https://www.googleapis.com/auth/gmail.modify
https://www.googleapis.com/auth/gmail.labels
```

### Token Refresh

The access token expires after ~1 hour. The server **refreshes it automatically** in two places:
1. At startup, if the cached token is expired.
2. **Mid-session**, on the first 401 / `RefreshError` from any tool — `_call()` refreshes once, persists the new token to `secrets/gmail_creds.json`, and retries the call.

If the refresh token itself has been revoked or expired, `status` and every tool return an actionable `Error:` instructing to re-run `scripts/gmail_oauth_setup.py`.

### Git Safety

`./secrets/` is in `.gitignore` — credentials are never committed.

---

## Setup

### 1. Create OAuth credentials

1. Go to [Google Cloud Console → Credentials](https://console.cloud.google.com/apis/credentials).
2. Create an **OAuth client ID** of type *Desktop app*.
3. Put `client_id` and `client_secret` in `secrets/google_oauth_client.json`:
   ```json
   { "client_id": "....apps.googleusercontent.com", "client_secret": "GOCSPX-..." }
   ```
4. Enable the **Gmail API** under APIs & Services.

(The OAuth client file is shared with gcal; a single Desktop client works for both.)

### 2. Run the OAuth flow

```bash
.venv/bin/python scripts/gmail_oauth_setup.py
```

Opens a browser, asks for the Gmail scopes, and saves the token to `secrets/gmail_creds.json`.

### 3. Install dependencies

`google-auth`, `google-auth-oauthlib`, `google-api-python-client` are already listed in `requirements.txt`. Re-running `./run.sh` installs them automatically; or:

```bash
uv pip install -r requirements.txt
```

### 4. Register the server with the agent

```
register_mcp(
  name="gmail",
  transport="stdio",
  command="python3",
  args=["scripts/gmail_mcp_server.py"]
)
```

---

## Usage Examples

### Self-check: is Gmail working?

```
mcp__gmail__status()
```
Returns `Status: READY ✅ (ok)` with the account email if credentials load, the token refreshes, and the Gmail API responds; otherwise a `Status: … ❌/⚠️ (…)` report with a `What to do:` list. Call this first whenever another gmail tool fails.

### List unread messages

```
mcp__gmail__list_messages(query="is:unread", max_results=10)
```

When more results exist, the response ends with a `More results available. Use page_token='...'` line. Pass that token back to page forward:

```
mcp__gmail__list_messages(query="is:unread", max_results=10, page_token="09876...")
```

### Read a message

```
mcp__gmail__get_message(message_id="190abc123...", include_body=true)
```

The body is the `text/plain` part when present; for HTML-only emails it is converted to readable text and labelled `--- Body (converted from HTML) ---`. If the message has attachments, their filenames are listed on an `Attachments:` line — download them with `download_attachments` (which only needs the `message_id`).

### Mark as read / archive

```
mcp__gmail__modify_message(message_id="190abc123...", remove_labels=["UNREAD"])
mcp__gmail__modify_message(message_id="190abc123...", remove_labels="INBOX")  # archive
```

### Send / reply in-thread

```
mcp__gmail__send_message(
  to="friend@example.com",
  subject="Hello!",
  body="How are you?"
)

mcp__gmail__send_message(
  to="sender@example.com",
  subject="Re: Original subject",
  body="This is my reply.",
  in_reply_to="190abc123...",
  thread_id="190thread456..."
)
```

`in_reply_to` adds RFC 2822 threading headers; `thread_id` attaches the message to the correct Gmail thread. For a proper reply visible in all clients, pass both.

### Send with attachments

```
mcp__gmail__send_message(
  to="friend@example.com",
  subject="Documents attached",
  body="See the two files attached.",
  attachments=[
    "data/gmail_attachments/PREVENTIVO DI SPESA.pdf",
    "uploads/gmail_attachments/00035.pdf"
  ]
)
```

`attachments` is an array of local file paths. Each path is either absolute or relative to the **project root**; the server reads every file from disk, guesses its MIME type, and adds it to a `multipart/mixed` message. **If any path does not exist the email is NOT sent** — the tool returns `Error: attachment not found: <path>` so the agent can correct it. Total attachment size is limited to ~25 MB (the Gmail `messages.send` request embeds the message as base64).

### Download attachments

```
mcp__gmail__download_attachments(message_id="190abc123...")
```
Saves into `data/gmail_attachments/` (served via `/data/gmail_attachments/...` in the frontend). Override with `folder`.

---

## Enable / Disable

```
toggle_item(kind="mcp", id="gmail", enabled=false)   # disable
toggle_item(kind="mcp", id="gmail", enabled=true)    # re-enable
restart                                    # required for changes to take effect
```

---

## Dependencies

| Package | Version | Purpose |
|---------|---------|---------|
| `google-api-python-client` | 2.197.0 | Google API Python client (Gmail v1) |
| `google-auth` | 2.55.0 | Auth / token refresh |
| `google-auth-oauthlib` | 1.4.0 | Local OAuth flow (setup script) |

---

## Error Handling

Every Google API exception is mapped to an actionable `Error:` string (flagged with `isError: true`):

| Condition | Response |
|-----------|----------|
| Credentials file missing | `"Error: Credentials file not found at …. Run scripts/gmail_oauth_setup.py…"` |
| Credentials invalid & no refresh token | `"Error: Credentials invalid and cannot be refreshed. Re-run scripts/gmail_oauth_setup.py."` |
| Token refresh failed / revoked | `"Error: Gmail API token refresh failed …. Re-run scripts/gmail_oauth_setup.py…"` |
| HTTP 401 | `"Error: Gmail API rejected the access token (401)…. Re-run scripts/gmail_oauth_setup.py…"` |
| HTTP 403 | `"Error: Gmail API returned 403 Forbidden. The OAuth scopes … are insufficient, or the Gmail API is disabled in the Google Cloud Console…"` |
| HTTP 404 | `"Error: Gmail API returned 404 Not Found. Check the message/thread/attachment ID."` |
| HTTP 429 | `"Error: Gmail API rate limit exceeded (429). Wait a moment and retry."` |
| HTTP 400 | `"Error: Gmail API rejected the request as invalid (400). Check the parameters. Detail: …"` |
| HTTP 5xx | `"Error: Gmail API returned a server error (HTTP …). Retry in a moment."` |
| Missing required param | `"Error: Missing required parameter '<name>'"` |
| Unknown tool | `"Error: Unknown tool: <name>"` |

All errors are logged to stderr with the `[gmail_mcp]` prefix.

---

## Protocol

Implements JSON-RPC 2.0 over stdio (same as gcal / gmaps / whatsapp servers):

- **Requests:** read from stdin, one JSON object per line
- **Responses:** written to stdout
- **Notifications:** server-initiated (`event/new_email`), no `id`
- **Logs:** stderr only, prefixed `[gmail_mcp]`

Supported methods: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`.

---

## When to Update This File

- A tool is added, removed, or renamed
- Auth mechanism or scopes change
- Credential path changes
- New error cases are mapped
- Protocol version changes
