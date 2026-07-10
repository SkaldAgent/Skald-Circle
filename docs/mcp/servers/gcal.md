# Google Calendar MCP Server (gcal)

## Overview

A Python MCP server providing **read + write** access to Google Calendar via the Google Calendar API v3.

**Server name:** `gcal`
**Transport:** `stdio` (spawns `python3 scripts/gcal_mcp_server.py`)
**Location:** `scripts/gcal_mcp_server.py`

The server also emits push notifications (`event/new_calendar_event`) to the agent when new events are created on the primary calendar (polled every 5 min).

---

## Tools

All tools are callable as `mcp__gcal__<tool>`; the table lists the bare `<tool>` names.

| Tool | Required params | Optional params | Description |
|------|-----------------|-----------------|-------------|
| `status` | *(none)* | *(none)* | Self-check: verifies credentials load, the token refreshes, and the Calendar API responds. Call first when another gcal tool fails. |
| `list_calendars` | *(none)* | *(none)* | List calendars accessible to the user (surfaces `calendar_id` values). |
| `list_events` | *(none)* | `calendar_id`, `time_min`, `time_max`, `max_results`, `full_text`, `time_zone` | Chronological event listing. `time_min` defaults to NOW. |
| `get_event` | `event_id` | `calendar_id` | Read a single event by ID, including attendees + RSVP status. |
| `create_event` | `summary`, `start`, `end` | `description`, `location`, `attendees`, `recurrence`, `time_zone`, `calendar_id`, `reminders` | Create an event; returns ID + HTML link. |
| `update_event` | `event_id` | `summary`, `start`, `end`, `description`, `location`, `attendees`, `time_zone`, `calendar_id`, `reminders` | Patch fields of an existing event; omitted fields are preserved. |
| `delete_event` | `event_id` | `calendar_id` | Permanently delete an event. Irreversible. |
| `respond_to_event` | `event_id`, `response` | `calendar_id` | Set RSVP (`accepted`, `declined`, `tentative`, `needsAction`). |

### `reminders` format

`reminders` (on `create_event` / `update_event`) accepts a JSON array whose items are **either** integers (minutes before the event, popup reminder) **or** objects `{"method": "popup"|"email", "minutes": <int>}`. Overrides calendar defaults. Examples: `[10, 30, 60]` or `[{"method": "email", "minutes": 60}, {"method": "popup", "minutes": 10}]`.

---

## Authentication

### Credentials File

OAuth 2.0 user credentials are stored in:
- **Default path:** `./secrets/google_creds.json` (relative to project root)
- **Override:** `GOOGLE_CREDS_PATH` env var

Required OAuth scope (granted by the setup script):
```
https://www.googleapis.com/auth/calendar
```

### Token Refresh

The access token expires after ~1 hour. The server **refreshes it automatically** in two places:
1. At startup, if the cached token is expired.
2. **Mid-session**, on the first 401 / `RefreshError` from any tool — `_call()` refreshes once, persists the new token to `secrets/google_creds.json`, and retries the call.

If the refresh token itself has been revoked or expired, `status` and every tool return an actionable `Error:` instructing to re-run `scripts/gcal_oauth_setup.py`.

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
4. Enable the **Google Calendar API** under APIs & Services.

### 2. Run the OAuth flow

```bash
.venv/bin/python scripts/gcal_oauth_setup.py
```

Opens a browser, asks for the Calendar scope, and saves the token to `secrets/google_creds.json`.

### 3. Install dependencies

`google-auth`, `google-auth-oauthlib`, `google-api-python-client` are already listed in `requirements.txt`. Re-running `./run.sh` installs them automatically; or:

```bash
uv pip install -r requirements.txt
```

### 4. Register the server with the agent

```
register_mcp(
  name="gcal",
  transport="stdio",
  command="python3",
  args=["scripts/gcal_mcp_server.py"]
)
```

---

## Usage Examples

### Self-check: is Calendar working?

```
mcp__gcal__status()
```
Returns `Status: READY ✅ (ok)` if credentials load, the token refreshes, and the Calendar API responds; otherwise a `Status: … ❌/⚠️ (…)` report with a `What to do:` list. Call this first whenever another gcal tool fails.

### List events in the next 7 days

```
mcp__gcal__list_events(
  calendar_id="primary",
  time_min="2026-06-22T00:00:00+02:00",
  time_max="2026-06-29T00:00:00+02:00",
  max_results=50,
  time_zone="Europe/Rome"
)
```
`time_min` / `time_max` are also accepted as `start_time` / `end_time` for backward compatibility. If `time_min` is omitted it defaults to NOW.

### Search events

```
mcp__gcal__list_events(
  full_text="dentist",
  time_min="2026-01-01T00:00:00+01:00",
  time_max="2026-12-31T00:00:00+01:00"
)
```

### Get / create / RSVP

```
mcp__gcal__get_event(event_id="<id>")

mcp__gcal__create_event(
  summary="Dentist",
  start="2026-07-01T09:00:00",
  end="2026-07-01T10:00:00",
  reminders=[30, {"method": "email", "minutes": 120}]
)

mcp__gcal__respond_to_event(event_id="<id>", response="accepted")
```

---

## Enable / Disable

```
toggle_item(kind="mcp", id="gcal", enabled=false)   # disable
toggle_item(kind="mcp", id="gcal", enabled=true)    # re-enable
restart                                    # required for changes to take effect
```

---

## Dependencies

| Package | Version | Purpose |
|---------|---------|---------|
| `google-api-python-client` | 2.197.0 | Google API Python client (Calendar v3) |
| `google-auth` | 2.55.0 | Auth / token refresh |
| `google-auth-oauthlib` | 1.4.0 | Local OAuth flow (setup script) |

---

## Error Handling

Every Google API exception is mapped to an actionable `Error:` string (flagged with `isError: true`):

| Condition | Response |
|-----------|----------|
| Credentials file missing | `"Error: Credentials file not found at …. Run scripts/gcal_oauth_setup.py…"` |
| Token refresh failed / revoked | `"Error: Calendar API token refresh failed …. Re-run scripts/gcal_oauth_setup.py…"` |
| HTTP 401 | `"Error: Calendar API rejected the access token (401)…. Re-run scripts/gcal_oauth_setup.py…"` |
| HTTP 403 | `"Error: Calendar API returned 403 Forbidden. The OAuth scopes … are insufficient, or the Calendar API is disabled in the Google Cloud Console…"` |
| HTTP 404 | `"Error: Calendar API returned 404 Not Found. Check the event/calendar ID…"` |
| HTTP 429 | `"Error: Calendar API rate limit exceeded (429). Wait a moment and retry."` |
| HTTP 400 | `"Error: Calendar API rejected the request as invalid (400). Check the parameters. Detail: …"` |
| HTTP 5xx | `"Error: Calendar API returned a server error (HTTP …). Retry in a moment."` |
| Missing required param | `"Error: Missing required parameter '<name>'"` |
| Unknown tool | `"Error: Unknown tool: <name>"` |

All errors are logged to stderr with the `[gcal_mcp]` prefix.

---

## Protocol

Implements JSON-RPC 2.0 over stdio (same as gmail / gmaps / whatsapp servers):

- **Requests:** read from stdin, one JSON object per line
- **Responses:** written to stdout
- **Notifications:** server-initiated (`event/new_calendar_event`), no `id`
- **Logs:** stderr only, prefixed `[gcal_mcp]`

Supported methods: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`.

---

## When to Update This File

- A tool is added, removed, or renamed
- Auth mechanism changes
- Credential path or scope changes
- New error cases are mapped
- Protocol version changes
