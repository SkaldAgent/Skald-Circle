#!/usr/bin/env python3
"""SerpAPI Google Flights MCP server (JSON-RPC 2.0 over stdio).

Capabilities:
  serpapi_search_flights — search one-way or round-trip flights via Google Flights
                            through SerpAPI, returning prices, airlines, durations,
                            layovers, and CO2 emissions.

Auth:
  API key is read from env var SERPAPI_API_KEY, or from the file at
  SERPAPI_API_KEY_FILE (default: ./secrets/serpapi_api_key.txt).

Run with:
  python3 scripts/mcp/serpapi_flights/server.py
"""

from __future__ import annotations

import json
import os
import re
import sys
from typing import Any

import httpx

# Log to stderr so stdout stays clean for JSON-RPC.
def log(msg: str) -> None:
    print(f"[serpapi_flights_mcp] {msg}", file=sys.stderr, flush=True)


# ── API key / client init ──────────────────────────────────────────────────────

SERPAPI_BASE_URL = "https://serpapi.com"
_DEFAULT_KEY_FILE = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "secrets",
    "serpapi_api_key.txt",
)

_init_error: str | None = None


def _get_api_key() -> str | None:
    # 1. Environment variable
    key = os.environ.get("SERPAPI_API_KEY", "").strip()
    if key:
        return key

    # 2. File
    key_file = os.environ.get("SERPAPI_API_KEY_FILE", _DEFAULT_KEY_FILE)
    if os.path.exists(key_file):
        try:
            with open(key_file) as f:
                key = f.read().strip()
            if key:
                return key
        except OSError as e:
            global _init_error
            _init_error = f"Failed to read API key file {key_file}: {e}"
            log(_init_error)
            return None

    _init_error = (
        "SerpAPI API key not found. "
        "Set SERPAPI_API_KEY env var or create secrets/serpapi_api_key.txt "
        "with just the key on the first line."
    )
    log(_init_error)
    return None


def _serpapi_request(params: dict) -> dict:
    """Make a synchronous GET to SerpAPI /search. Raises on HTTP errors."""
    api_key = _get_api_key()
    if not api_key:
        raise _InitError(_init_error or "SerpAPI API key not configured.")

    full_params = {"api_key": api_key, **params}
    with httpx.Client(timeout=30.0, headers={"User-Agent": "skald-serpapi-mcp/2.0"}) as client:
        response = client.get(f"{SERPAPI_BASE_URL}/search", params=full_params)
        response.raise_for_status()
        return response.json()


class _InitError(Exception):
    """Raised when the API key is missing or unreadable."""


# ── Error mapping ──────────────────────────────────────────────────────────────

def _format_api_error(e: Exception) -> str:
    if isinstance(e, _InitError):
        return f"Error: {e}"
    if isinstance(e, httpx.HTTPStatusError):
        status = e.response.status_code
        if status == 401:
            return "Error: Invalid SerpAPI API key. Check secrets/serpapi_api_key.txt or the SERPAPI_API_KEY env var."
        if status == 429:
            return "Error: SerpAPI rate limit exceeded. Wait a moment and retry."
        if status == 400:
            return f"Error: Bad request — {e.response.text[:200]}. Verify airport codes (3-letter IATA) and dates."
        return f"Error: SerpAPI request failed (HTTP {status})."
    if isinstance(e, httpx.TimeoutException):
        return "Error: Request to SerpAPI timed out (30s). The service may be slow or unreachable; retry."
    if isinstance(e, httpx.RequestError):
        return f"Error: Network error contacting SerpAPI: {e}"
    return f"Error: Unexpected error: {type(e).__name__}: {e}"


# ── Output formatting ──────────────────────────────────────────────────────────

def _format_flight_results(data: dict, max_results: int) -> str:
    """Render SerpAPI Google Flights results as plain text for the LLM."""
    best_flights = data.get("best_flights", []) or []
    other_flights = data.get("other_flights", []) or []
    price_insights = data.get("price_insights") or {}

    if not best_flights and not other_flights:
        return "No flights found for the given route and dates."

    lines: list[str] = []

    if price_insights:
        pi = price_insights
        if pi.get("lowest_price"):
            lines.append(f"Lowest price: {pi['lowest_price']}")
        if pi.get("typical_price_range"):
            lo, hi = pi["typical_price_range"][0], pi["typical_price_range"][1]
            lines.append(f"Typical range: {lo} – {hi}")
        if lines:
            lines.append("")

    lines.append("Flights:")
    lines.append("")

    all_flights = (best_flights + other_flights)[:max_results]

    for i, flight in enumerate(all_flights, 1):
        segments = flight.get("flights", []) or []
        total_duration = flight.get("total_duration", 0)  # minutes
        price = flight.get("price", 0)
        layovers = flight.get("layovers", []) or []

        hours, minutes = divmod(total_duration, 60)
        duration_str = f"{hours}h {minutes}m" if hours > 0 else f"{minutes}m"

        cheapest_tag = "  (CHEAPEST)" if i == 1 and best_flights else ""
        lines.append(f"#{i}: {price}{cheapest_tag}")
        lines.append("")

        for seg in segments:
            dep = seg.get("departure_airport", {}) or {}
            arr = seg.get("arrival_airport", {}) or {}
            airline = seg.get("airline", "?")
            flight_num = seg.get("flight_number", "?")
            seg_dur = seg.get("duration", 0)
            seg_h, seg_m = divmod(seg_dur, 60)
            seg_dur_str = f"{seg_h}h {seg_m}m" if seg_h > 0 else f"{seg_m}m"

            lines.append(f"  {airline} {flight_num}")
            lines.append(f"  {dep.get('id', '?')} {dep.get('time', '?')} -> {arr.get('id', '?')} {arr.get('time', '?')}")
            lines.append(f"  Duration: {seg_dur_str}")
            lines.append("")

        if layovers:
            parts = []
            for lo in layovers:
                lo_dur = lo.get("duration", 0)
                lo_h, lo_m = divmod(lo_dur, 60)
                parts.append(f"{lo.get('id', '?')} ({lo_h}h {lo_m}m)")
            lines.append(f"  Layovers: {' -> '.join(parts)}")
            lines.append("")

        lines.append(f"  Total duration: {duration_str}")

        emissions = flight.get("carbon_emissions") or {}
        if emissions.get("this_flight") is not None:
            lines.append(f"  CO2: {emissions['this_flight']}g")

        lines.append("")

    return "\n".join(lines).rstrip()


# ── Tool implementation ────────────────────────────────────────────────────────

_DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
_IATA_RE = re.compile(r"^[A-Za-z]{3}$")
_VALID_CABINS = {"economy", "premium_economy", "business", "first"}


def _validate_int(value: Any, name: str, lo: int, hi: int, default: int) -> int:
    if value is None:
        return default
    try:
        v = int(value)
    except (TypeError, ValueError):
        raise _ValidationError(f"'{name}' must be an integer between {lo} and {hi}.")
    if v < lo or v > hi:
        raise _ValidationError(f"'{name}' must be between {lo} and {hi} (got {v}).")
    return v


class _ValidationError(Exception):
    """Raised for invalid tool arguments; message is returned to the LLM."""


def _serpapi_search_flights(args: dict) -> str:
    # ── Required params ────────────────────────────────────────────────────────
    departure_id = (args.get("departure_id") or "").strip().upper()
    arrival_id = (args.get("arrival_id") or "").strip().upper()
    outbound_date = (args.get("outbound_date") or "").strip()

    if not departure_id:
        raise _ValidationError("Missing required parameter 'departure_id' (3-letter IATA airport or city code, e.g. 'JFK', 'MIL').")
    if not _IATA_RE.match(departure_id):
        raise _ValidationError(f"'departure_id' must be exactly 3 ASCII letters (got '{departure_id}'). Use an airport code (e.g. 'JFK') or a city code (e.g. 'NYC', 'MIL', 'LON').")
    if not arrival_id:
        raise _ValidationError("Missing required parameter 'arrival_id' (3-letter IATA airport or city code).")
    if not _IATA_RE.match(arrival_id):
        raise _ValidationError(f"'arrival_id' must be exactly 3 ASCII letters (got '{arrival_id}'). Use an airport code (e.g. 'FCO') or a city code (e.g. 'ROM').")
    if not outbound_date:
        raise _ValidationError("Missing required parameter 'outbound_date' (YYYY-MM-DD).")
    if not _DATE_RE.match(outbound_date):
        raise _ValidationError(f"'outbound_date' must be in YYYY-MM-DD format (got '{outbound_date}').")

    # ── Optional params ────────────────────────────────────────────────────────
    return_date = (args.get("return_date") or "").strip() or None
    if return_date and not _DATE_RE.match(return_date):
        raise _ValidationError(f"'return_date' must be in YYYY-MM-DD format (got '{return_date}').")

    adults = _validate_int(args.get("adults"), "adults", 1, 10, 1)
    children = _validate_int(args.get("children"), "children", 0, 8, 0)
    infants_in_seat = _validate_int(args.get("infants_in_seat"), "infants_in_seat", 0, 4, 0)
    infants_on_lap = _validate_int(args.get("infants_on_lap"), "infants_on_lap", 0, 4, 0)

    stops_raw = args.get("stops")
    stops: int | None = None
    if stops_raw is not None:
        try:
            stops = int(stops_raw)
        except (TypeError, ValueError):
            raise _ValidationError("'stops' must be 0 (non-stop only), 1 (max 1 stop), or 2 (max 2 stops).")
        if stops not in (0, 1, 2):
            raise _ValidationError(f"'stops' must be 0, 1, or 2 (got {stops}).")

    currency = (args.get("currency") or "EUR").strip().upper()
    if not re.match(r"^[A-Z]{3}$", currency):
        raise _ValidationError(f"'currency' must be a 3-letter ISO code (got '{currency}').")

    preferred_cabins = args.get("preferred_cabins")
    if preferred_cabins is not None:
        preferred_cabins = str(preferred_cabins).strip().lower()
        if preferred_cabins not in _VALID_CABINS:
            raise _ValidationError(f"'preferred_cabins' must be one of: {', '.join(sorted(_VALID_CABINS))}.")

    hl = args.get("hl") or "en"

    max_results = _validate_int(args.get("max_results"), "max_results", 1, 50, 10)

    # ── Build SerpAPI params ───────────────────────────────────────────────────
    api_params: dict[str, Any] = {
        "engine": "google_flights",
        "departure_id": departure_id,
        "arrival_id": arrival_id,
        "outbound_date": outbound_date,
        "adults": adults,
        "children": children,
        "infants_in_seat": infants_in_seat,
        "infants_on_lap": infants_on_lap,
        "currency": currency,
        "hl": hl,
    }
    if return_date:
        api_params["return_date"] = return_date
        api_params["type"] = "1"  # round-trip
    else:
        api_params["type"] = "2"  # one-way
    if stops is not None:
        api_params["stops"] = stops
    if preferred_cabins:
        api_params["preferred_cabins"] = preferred_cabins

    # ── Call ───────────────────────────────────────────────────────────────────
    try:
        data = _serpapi_request(api_params)
    except Exception as e:
        return _format_api_error(e)

    if data.get("error"):
        return f"Error: SerpAPI returned an error: {data['error']}"

    return _format_flight_results(data, max_results)


# ── Tool manifest ──────────────────────────────────────────────────────────────

TOOLS = [
    {
        "name": "serpapi_search_flights",
        "description": (
            "Search one-way or round-trip flights on Google Flights via SerpAPI. "
            "Returns a plain-text list of routes with price, airline + flight number, "
            "departure/arrival times, segment durations, total duration, layovers, "
            "and CO2 emissions. The first result is typically the cheapest.\n"
            "Both airport codes (3 letters, e.g. 'JFK', 'FCO') and city codes "
            "(3 letters covering all airports of a city, e.g. 'NYC', 'ROM', 'MIL', "
            "'LON') are accepted for departure_id and arrival_id — prefer city codes "
            "when the user does not name a specific airport."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "departure_id": {
                    "type": "string",
                    "description": "Departure airport or city code — exactly 3 ASCII letters. Examples: 'JFK' (New York JFK), 'MIL' (any Milan airport), 'LON' (any London airport), 'ROM' (any Rome airport).",
                },
                "arrival_id": {
                    "type": "string",
                    "description": "Arrival airport or city code — exactly 3 ASCII letters. See departure_id for examples.",
                },
                "outbound_date": {
                    "type": "string",
                    "description": "Outbound date in YYYY-MM-DD format (e.g. '2026-08-01').",
                },
                "return_date": {
                    "type": "string",
                    "description": "Return date in YYYY-MM-DD for round-trip searches. Omit for one-way.",
                },
                "adults": {
                    "type": "integer",
                    "description": "Number of adult passengers (12+). Default 1, max 10.",
                },
                "children": {
                    "type": "integer",
                    "description": "Number of children (2-11). Default 0, max 8.",
                },
                "infants_in_seat": {
                    "type": "integer",
                    "description": "Number of infants occupying a seat. Default 0, max 4.",
                },
                "infants_on_lap": {
                    "type": "integer",
                    "description": "Number of infants on an adult's lap (under 2). Default 0, max 4.",
                },
                "stops": {
                    "type": "integer",
                    "enum": [0, 1, 2],
                    "description": "Maximum number of stops: 0 = non-stop only, 1 = max 1 stop, 2 = max 2 stops. Omit to allow any.",
                },
                "currency": {
                    "type": "string",
                    "description": "ISO 4217 currency code for prices (e.g. 'EUR', 'USD', 'GBP'). Default 'EUR'.",
                },
                "preferred_cabins": {
                    "type": "string",
                    "enum": ["economy", "premium_economy", "business", "first"],
                    "description": "Cabin class filter. Omit to search all cabins.",
                },
                "hl": {
                    "type": "string",
                    "description": "Language code for results (e.g. 'en', 'it', 'fr'). Default 'en'.",
                },
                "max_results": {
                    "type": "integer",
                    "description": "Maximum number of flight options to return. Default 10, max 50.",
                },
            },
            "required": ["departure_id", "arrival_id", "outbound_date"],
        },
    },
]


# ── JSON-RPC dispatch ──────────────────────────────────────────────────────────

TOOL_DISPATCH = {
    "serpapi_search_flights": _serpapi_search_flights,
}


def _ok(req_id: Any, result: Any) -> str:
    return json.dumps({"jsonrpc": "2.0", "id": req_id, "result": result})


def _text_result(req_id: Any, text: str, is_error: bool = False) -> str:
    payload: dict = {
        "jsonrpc": "2.0",
        "id": req_id,
        "result": {"content": [{"type": "text", "text": text}]},
    }
    if is_error:
        payload["result"]["isError"] = True
    return json.dumps(payload)


def handle_request(msg: dict) -> str | None:
    method = msg.get("method", "")
    req_id = msg.get("id")

    if method == "initialize":
        return _ok(req_id, {
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {
                "name": "serpapi_flights",
                "version": "2.0.0",
            },
        })

    if method == "notifications/initialized":
        return None

    if method == "tools/list":
        return _ok(req_id, {"tools": TOOLS})

    if method == "tools/call":
        params = msg.get("params", {})
        tool_name = params.get("name", "")
        tool_args = params.get("arguments", {}) or {}

        handler = TOOL_DISPATCH.get(tool_name)
        if handler is None:
            return _text_result(req_id, f"Error: Unknown tool: {tool_name}", is_error=True)

        try:
            text = handler(tool_args)
        except _ValidationError as e:
            return _text_result(req_id, f"Error: {e}", is_error=True)
        except Exception as e:
            log(f"Unhandled exception in tool '{tool_name}': {e}")
            return _text_result(req_id, f"Error: Internal error in tool '{tool_name}': {e}", is_error=True)

        is_err = text.startswith("Error:")
        return _text_result(req_id, text, is_error=is_err)

    return json.dumps({
        "jsonrpc": "2.0",
        "id": req_id,
        "error": {"code": -32601, "message": f"Method not found: {method}"},
    })


# ── Main loop ──────────────────────────────────────────────────────────────────

def main() -> None:
    log("Starting SerpAPI Google Flights MCP server")
    # Validate API key eagerly so configuration errors surface at startup.
    _get_api_key()
    try:
        for line in sys.stdin:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError as e:
                log(f"Invalid JSON input: {e}")
                continue

            resp = handle_request(msg)
            if resp is not None:
                sys.stdout.write(resp + "\n")
                sys.stdout.flush()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
