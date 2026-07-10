#!/usr/bin/env python3
"""Download an iCal feed and output events as JSON."""

import argparse
import json
import sys
from datetime import datetime, timedelta, timezone
from typing import Any, Dict, List, Optional
from urllib.request import urlopen

from icalendar import Calendar


def parse_dt(value: Any) -> Optional[str]:
    """Parse an iCal date/datetime value and return ISO 8601 string."""
    if value is None:
        return None
    dt = value.dt
    if isinstance(dt, datetime):
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return dt.isoformat()
    # date-only (all-day events)
    return dt.isoformat()


def extract_attachments(event: Any) -> List[Dict[str, str]]:
    """Extract ATTACH properties as a list of {url, type} dicts."""
    attachments: List[Dict[str, str]] = []
    if "ATTACH" in event:
        # May be a single value or a list
        items = event["ATTACH"] if isinstance(event["ATTACH"], list) else [event["ATTACH"]]
        for item in items:
            attach: Dict[str, str] = {"url": str(item)}
            params = item.params
            if "FMTTYPE" in params:
                attach["type"] = params["FMTTYPE"]
            if "FILENAME" in params:
                attach["filename"] = params["FILENAME"]
            attachments.append(attach)
    return attachments


def extract_text(component: Any, key: str) -> Optional[str]:
    """Extract a text property, decoded from any encoding."""
    raw = component.get(key)
    if raw is None:
        return None
    # vCategory objects have a .cats attribute (list of vText)
    if hasattr(raw, "cats"):
        return ", ".join(str(item) for item in raw.cats)
    # Other list-like properties
    if isinstance(raw, list):
        return ", ".join(str(item) for item in raw)
    return str(raw)


def event_to_dict(event: Any) -> Dict[str, Any]:
    """Convert an iCal VEVENT to a flat dict."""
    return {
        "uid": extract_text(event, "UID"),
        "title": extract_text(event, "SUMMARY"),
        "description": extract_text(event, "DESCRIPTION"),
        "location": extract_text(event, "LOCATION"),
        "where": extract_text(event, "X-TEAMUP-WHERE"),
        "categories": extract_text(event, "CATEGORIES"),
        "event_url": extract_text(event, "URL"),
        "external_url": extract_text(event, "X-TEAMUP-EVENT-URL"),
        "start": parse_dt(event.get("DTSTART")),
        "end": parse_dt(event.get("DTEND")),
        "created": parse_dt(event.get("CREATED")),
        "last_modified": parse_dt(event.get("LAST-MODIFIED")),
        "stamp": parse_dt(event.get("DTSTAMP")),
        "attachments": extract_attachments(event),
    }


def download_feed(url: str) -> Calendar:
    """Download and parse an iCal feed."""
    with urlopen(url) as response:
        raw = response.read()
    return Calendar.from_ical(raw)


def _parse_iso(iso: str) -> datetime:
    """Parse an ISO 8601 string, making it UTC-aware."""
    dt = datetime.fromisoformat(iso)
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def is_future(event_dict: Dict[str, Any], now: datetime) -> bool:
    """Check if an event is in the future (or ongoing)."""
    end = event_dict.get("end")
    if end is None:
        start = event_dict.get("start")
        if start is None:
            return True
        return _parse_iso(start) >= now
    return _parse_iso(end) >= now


def is_within_days(event_dict: Dict[str, Any], days: int, now: datetime) -> bool:
    """Check if an event starts within the next N days."""
    start = event_dict.get("start")
    if start is None:
        return True
    dt = _parse_iso(start)
    cutoff = now + timedelta(days=days)
    return dt >= now and dt <= cutoff


def main() -> None:
    parser = argparse.ArgumentParser(description="Download an iCal feed and output events as JSON")
    parser.add_argument("url", help="URL of the iCal feed (.ics)")
    parser.add_argument("--days", type=int, default=None, help="Only return events starting within the next N days")
    parser.add_argument("--all", action="store_true", help="Include past events (default: future only)")
    parser.add_argument("--pretty", action="store_true", help="Pretty-print JSON output (default: compact)")
    parser.add_argument("--meta", action="store_true", help="Only output calendar metadata (no events)")
    args = parser.parse_args()

    try:
        cal = download_feed(args.url)
    except Exception as e:
        print(f"Error downloading feed: {e}", file=sys.stderr)
        sys.exit(1)

    now = datetime.now(timezone.utc)

    # Calendar-level metadata
    cal_name = extract_text(cal, "X-WR-CALNAME") or extract_text(cal, "SUMMARY") or "Unknown"
    cal_desc = extract_text(cal, "X-WR-CALDESC") or extract_text(cal, "DESCRIPTION") or ""

    # Count total events (all of them, unfiltered)
    total_all = sum(1 for c in cal.walk() if c.name == "VEVENT")

    output: Dict[str, Any] = {
        "calendar": cal_name,
        "description": cal_desc,
        "feed_url": args.url,
        "fetched_at": now.isoformat(),
        "total_events": total_all,
    }

    if args.meta:
        if args.pretty:
            print(json.dumps(output, indent=2, ensure_ascii=False))
        else:
            print(json.dumps(output, ensure_ascii=False))
        return

    events: List[Dict[str, Any]] = []
    for component in cal.walk():
        if component.name != "VEVENT":
            continue
        ev = event_to_dict(component)

        # Apply filters
        if not args.all and not is_future(ev, now):
            continue
        if args.days is not None and not is_within_days(ev, args.days, now):
            continue

        events.append(ev)

    # Sort by start date (ascending, None at the end)
    events.sort(key=lambda e: e.get("start") or "9999")

    output["total_events"] = len(events)
    output["events"] = events

    if args.pretty:
        print(json.dumps(output, indent=2, ensure_ascii=False))
    else:
        print(json.dumps(output, ensure_ascii=False))


if __name__ == "__main__":
    main()
