---
name: ics2json
description: Download an iCalendar (ICS) feed from a URL and output structured JSON with all events. Use this skill whenever the user needs to read, analyze, or integrate events from a public iCal feed such as Teamup or Google Calendar, especially when no API keys are available. Also use as the base for cron jobs that periodically analyze a calendar feed. If the user mentions an .ics file or calendar feed, use this skill.
---

# ics2json

_Updated: 2026-06-08_

Downloads an iCalendar (ICS) feed from a URL and outputs structured JSON with all events.

## When to use

- Reading and analyzing events from a public iCal feed (Teamup, Google Calendar, etc.)
- Integrating an external calendar without API keys
- As the base for cron jobs that periodically analyze a feed

## Script

`python3 skills/ics2json/ics2json.py <url> [options]`

### Options

| Flag | Description |
|------|-------------|
| `--days N` | Only events starting within the next N days |
| `--all` | Include past events (default: future or ongoing only) |
| `--pretty` | Pretty-print JSON output (default: compact, single-line) |
| `--meta` | Only output calendar metadata (name, description, event count). No events returned. |

### Output JSON format

```json
{
  "calendar": "Calendar name",
  "description": "Calendar description",
  "feed_url": "Feed URL",
  "fetched_at": "2026-05-20T12:00:00+01:00",
  "total_events": 5,
  "events": [
    {
      "uid": "TU123456",
      "title": "Event title",
      "description": "Description...",
      "location": "Venue",
      "where": "Teamup address",
      "categories": "kink oriented event ⛓️",
      "event_url": "https://teamup.com/...",
      "external_url": "https://tickets.example.com",
      "start": "2026-05-20T19:00:00+01:00",
      "end": "2026-05-20T23:00:00+01:00",
      "created": "2026-05-01T10:00:00+00:00",
      "last_modified": null,
      "stamp": "2026-05-19T13:18:47+00:00",
      "attachments": [{"url": "https://...", "type": "image/jpeg", "filename": "flyer.jpg"}]
    }
  ]
}
```

### Examples

```bash
# Download a Teamup feed, future events only (compact output)
python3 skills/ics2json/ics2json.py https://ics.teamup.com/feed/ksmt7zqvai72zisjo4/12645979.ics

# Metadata only (compact)
python3 skills/ics2json/ics2json.py https://ics.teamup.com/feed/ksmt7zqvai72zisjo4/12645979.ics --meta

# Only the next 30 days (compact)
python3 skills/ics2json/ics2json.py https://ics.teamup.com/feed/ksmt7zqvai72zisjo4/12645979.ics --days 30

# All events (including past), pretty-printed for human reading
python3 skills/ics2json/ics2json.py https://ics.teamup.com/feed/ksmt7zqvai72zisjo4/12645979.ics --all --pretty
```
