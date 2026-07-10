#!/usr/bin/env python3
"""Google Maps MCP server (JSON-RPC 2.0 over stdio).

Capabilities (callable as `mcp__gmaps__<tool>`):
  directions       — transit/driving/walking directions from A to B
  geocode          — convert an address or place name to coordinates
  reverse_geocode  — convert coordinates to an address
  search_places    — find nearby places (stations, stops, POIs)
  distance_matrix  — travel time & distance between multiple origins/destinations

Auth:
  API key is read from env var GOOGLE_MAPS_API_KEY, or from the file at
  GOOGLE_MAPS_API_KEY_FILE (default: ./secrets/gmaps_api_key.txt).

Required Google Cloud APIs to enable:
  - Directions API
  - Geocoding API
  - Places API (New) or Places API
  - Distance Matrix API

Run with:
  python3 scripts/gmaps_mcp_server.py
"""

from __future__ import annotations

import json
import os
import sys
from datetime import datetime, timezone
from typing import Any

# Log to stderr so stdout stays clean for JSON-RPC.
def log(msg: str) -> None:
    print(f"[gmaps_mcp] {msg}", file=sys.stderr, flush=True)


# ── API key / client init ──────────────────────────────────────────────────────

_client = None
_init_error: str | None = None


def _get_api_key() -> str | None:
    # 1. Environment variable
    key = os.environ.get("GOOGLE_MAPS_API_KEY", "").strip()
    if key:
        return key

    # 2. File
    key_file = os.environ.get(
        "GOOGLE_MAPS_API_KEY_FILE",
        os.path.join(
            os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
            "secrets",
            "gmaps_api_key.txt",
        ),
    )
    if os.path.exists(key_file):
        with open(key_file) as f:
            key = f.read().strip()
        if key:
            return key

    return None


def _get_client():
    global _client, _init_error
    if _client is not None:
        return _client

    try:
        import googlemaps  # type: ignore
    except ImportError as e:
        _init_error = f"Missing dependency: {e}. Run: pip install googlemaps"
        log(_init_error)
        return None

    api_key = _get_api_key()
    if not api_key:
        _init_error = (
            "Google Maps API key not found. "
            "Set GOOGLE_MAPS_API_KEY env var or create secrets/gmaps_api_key.txt "
            "with just the key on the first line."
        )
        log(_init_error)
        return None

    try:
        _client = googlemaps.Client(key=api_key)
        log("Google Maps client initialised successfully.")
        return _client
    except Exception as e:
        _init_error = f"Failed to build Maps client: {e}"
        log(_init_error)
        return None


def _format_gmaps_error(e: Exception, api_label: str) -> str:
    """Map a googlemaps exception into an actionable Error: string.

    `api_label` is a human name for the failing API (e.g. "Directions", "Geocoding"),
    used to point the user at the right Google Cloud Console switch.
    """
    try:
        from googlemaps import exceptions as gm_exc  # type: ignore
    except ImportError:
        gm_exc = None  # type: ignore

    if gm_exc is not None and isinstance(e, gm_exc.ApiError):
        status = getattr(e, "status", "") or ""
        message = (getattr(e, "message", "") or "").strip()
        if status == "OVER_QUERY_LIMIT":
            return (
                f"Error: {api_label} API quota exceeded (OVER_QUERY_LIMIT). "
                "Check usage and billing in the Google Cloud Console."
            )
        if status == "REQUEST_DENIED":
            return (
                f"Error: {api_label} API request denied (REQUEST_DENIED). "
                "Verify that the API key in secrets/gmaps_api_key.txt is valid and that "
                f"the {api_label} API is enabled in the Google Cloud Console."
            )
        if status == "INVALID_REQUEST":
            return (
                f"Error: {api_label} API rejected the request as invalid (INVALID_REQUEST). "
                "Check that the addresses, coordinates, and parameters are well-formed."
            )
        if status == "MAX_ELEMENTS_EXCEEDED":
            return (
                f"Error: {api_label} API returned MAX_ELEMENTS_EXCEEDED — too many "
                "origins×destinations at once. Reduce the input size and retry."
            )
        if status == "NOT_FOUND":
            return (
                f"Error: {api_label} API could not geocode one of the supplied places. "
                "Use more specific place names or coordinates."
            )
        return f"Error: {api_label} API error ({status}): {message}"

    if gm_exc is not None and isinstance(e, gm_exc.HTTPError):
        status = getattr(e, "status", "") or ""
        return f"Error: {api_label} API returned HTTP error {status}."

    if gm_exc is not None and isinstance(e, gm_exc.Timeout):
        return f"Error: {api_label} API request timed out. Retry in a moment."

    return f"Error: {api_label} API call failed: {e}"


# ── Tool implementations ───────────────────────────────────────────────────────

def _maps_status(args: dict) -> str:
    """Self-check: confirm the API key is present, valid, and the network works.

    Performs one cheap geocode ("Rome, IT") so we exercise key validation, the
    Geocoding API, and the network in a single round-trip. Returns a plain-text
    report the LLM can use to decide what to tell the user.
    """
    # Step 1: API key present?
    api_key = _get_api_key()
    if not api_key:
        return (
            "Error: Google Maps API key not found. "
            "Set GOOGLE_MAPS_API_KEY env var or create secrets/gmaps_api_key.txt "
            "with the key on the first line. No Google Maps tool will work until this is fixed."
        )

    # Step 2: dependency present + client built?
    gmaps = _get_client()
    if gmaps is None:
        return f"Error: {_init_error}"

    # Step 3: live call. One geocode is the cheapest "is the key valid?" probe.
    try:
        result = gmaps.geocode("Rome, IT")
    except Exception as e:
        return _format_gmaps_error(e, "Geocoding")

    if not result:
        return (
            "Error: Geocoding API returned no result for the probe query. "
            "The API key may be restricted or the Geocoding API may be disabled."
        )

    return (
        "OK: Google Maps client is ready. API key is present and the Geocoding API responds.\n"
        "All tools (directions, geocode, reverse_geocode, search_places, distance_matrix) are operational."
    )


def _maps_directions(args: dict) -> str:
    """Get directions from origin to destination."""
    gmaps = _get_client()
    if gmaps is None:
        return f"Error: {_init_error}"

    origin = args.get("origin")
    destination = args.get("destination")
    if not origin or not destination:
        return "Error: Missing required parameters 'origin' and/or 'destination'."

    mode = args.get("mode", "transit").lower()
    valid_modes = {"driving", "walking", "bicycling", "transit"}
    if mode not in valid_modes:
        return f"Error: 'mode' must be one of: {', '.join(sorted(valid_modes))}."

    # Optional departure time: literal "now" or an ISO 8601 datetime string.
    # Integers (Unix timestamps) are rejected explicitly — the schema documents
    # strings only and silently coercing ints would teach the LLM the wrong call.
    departure_raw = args.get("departure_time", "now")
    if isinstance(departure_raw, bool):
        return "Error: 'departure_time' must be 'now' or an ISO 8601 string (e.g. '2025-06-15T08:30:00+02:00'). Never pass a boolean."
    if isinstance(departure_raw, (int, float)):
        return "Error: 'departure_time' must be 'now' or an ISO 8601 string (e.g. '2025-06-15T08:30:00+02:00'). Never pass a Unix timestamp integer."
    if departure_raw == "now":
        departure_time = datetime.now(timezone.utc)
    else:
        try:
            departure_time = datetime.fromisoformat(str(departure_raw).replace("Z", "+00:00"))
        except ValueError:
            return (
                "Error: 'departure_time' must be the literal 'now' or an ISO 8601 datetime "
                f"string with timezone offset (e.g. '2025-06-15T08:30:00+02:00'). Got: {departure_raw!r}."
            )

    # Transit preferences
    transit_mode = args.get("transit_mode")       # e.g. "bus", "rail", "subway", "train", "tram"
    transit_routing_preference = args.get("transit_routing_preference")  # "less_walking", "fewer_transfers"
    language = args.get("language", "it")
    alternatives = args.get("alternatives", False)

    kwargs: dict[str, Any] = {
        "origin": origin,
        "destination": destination,
        "mode": mode,
        "language": language,
        "alternatives": alternatives,
    }
    if mode == "transit":
        kwargs["departure_time"] = departure_time
        if transit_mode:
            kwargs["transit_mode"] = transit_mode if isinstance(transit_mode, list) else [transit_mode]
        if transit_routing_preference:
            kwargs["transit_routing_preference"] = transit_routing_preference

    try:
        result = gmaps.directions(**kwargs)
    except Exception as e:
        return _format_gmaps_error(e, "Directions")

    if not result:
        return f"No routes found from '{origin}' to '{destination}'."

    lines = []
    for route_idx, route in enumerate(result):
        if alternatives and len(result) > 1:
            lines.append(f"\n── Route {route_idx + 1} of {len(result)} ──")
        summary = route.get("summary", "")
        if summary:
            lines.append(f"Via: {summary}")

        legs = route.get("legs", [])
        for leg in legs:
            duration = leg.get("duration", {}).get("text", "?")
            distance = leg.get("distance", {}).get("text", "?")
            dep_addr = leg.get("start_address", origin)
            arr_addr = leg.get("end_address", destination)
            dep_time = leg.get("departure_time", {}).get("text", "")
            arr_time = leg.get("arrival_time", {}).get("text", "")

            lines.append(f"From: {dep_addr}")
            lines.append(f"To:   {arr_addr}")
            lines.append(f"Duration: {duration}  |  Distance: {distance}")
            if dep_time:
                lines.append(f"Departure: {dep_time}  →  Arrival: {arr_time}")

            lines.append("\nSteps:")
            for step in leg.get("steps", []):
                instr = step.get("html_instructions", "")
                # Strip basic HTML tags for clean text output
                import re
                instr = re.sub(r"<[^>]+>", " ", instr).strip()
                instr = re.sub(r"\s+", " ", instr)

                step_dur  = step.get("duration", {}).get("text", "")
                step_dist = step.get("distance", {}).get("text", "")
                travel_mode = step.get("travel_mode", "")

                prefix = ""
                if travel_mode == "TRANSIT":
                    td = step.get("transit_details", {})
                    line_info  = td.get("line", {})
                    vehicle    = line_info.get("vehicle", {}).get("name", "")
                    line_name  = line_info.get("short_name") or line_info.get("name", "")
                    dep_stop   = td.get("departure_stop", {}).get("name", "")
                    arr_stop   = td.get("arrival_stop", {}).get("name", "")
                    dep_t      = td.get("departure_time", {}).get("text", "")
                    arr_t      = td.get("arrival_time", {}).get("text", "")
                    num_stops  = td.get("num_stops", "")
                    headsign   = td.get("headsign", "")
                    prefix = (
                        f"  🚌 {vehicle} {line_name}"
                        + (f" → {headsign}" if headsign else "")
                        + f"\n     From: {dep_stop} ({dep_t})"
                        + f"\n     To:   {arr_stop} ({arr_t})"
                        + (f"  [{num_stops} stops]" if num_stops else "")
                    )
                else:
                    emoji = {"WALKING": "🚶", "DRIVING": "🚗", "BICYCLING": "🚲"}.get(travel_mode, "•")
                    prefix = f"  {emoji} {instr}"
                    if step_dur or step_dist:
                        prefix += f"  ({step_dur}, {step_dist})"

                lines.append(prefix)

    return "\n".join(lines)


def _maps_geocode(args: dict) -> str:
    """Convert an address or place name to coordinates."""
    gmaps = _get_client()
    if gmaps is None:
        return f"Error: {_init_error}"

    address = args.get("address")
    if not address:
        return "Error: Missing required parameter 'address'."

    language = args.get("language", "it")
    region   = args.get("region", "it")  # country bias

    try:
        result = gmaps.geocode(address, language=language, region=region)
    except Exception as e:
        return _format_gmaps_error(e, "Geocoding")

    if not result:
        return f"No results found for '{address}'."

    lines = []
    for i, place in enumerate(result[:5]):
        formatted = place.get("formatted_address", "?")
        loc = place.get("geometry", {}).get("location", {})
        lat = loc.get("lat", "?")
        lng = loc.get("lng", "?")
        place_id = place.get("place_id", "")
        types = ", ".join(place.get("types", []))
        lines.append(f"{i+1}. {formatted}")
        lines.append(f"   Coordinates: {lat}, {lng}")
        if place_id:
            lines.append(f"   Place ID: {place_id}")
        if types:
            lines.append(f"   Types: {types}")

    return "\n".join(lines)


def _maps_reverse_geocode(args: dict) -> str:
    """Convert coordinates to an address."""
    gmaps = _get_client()
    if gmaps is None:
        return f"Error: {_init_error}"

    lat = args.get("lat")
    lng = args.get("lng")
    if lat is None or lng is None:
        return "Error: Missing required parameters 'lat' and/or 'lng'."

    language = args.get("language", "it")

    try:
        result = gmaps.reverse_geocode((float(lat), float(lng)), language=language)
    except Exception as e:
        return _format_gmaps_error(e, "Geocoding")

    if not result:
        return f"No address found for coordinates ({lat}, {lng})."

    place = result[0]
    return place.get("formatted_address", "?")


def _maps_search_places(args: dict) -> str:
    """Search for places near a location."""
    gmaps = _get_client()
    if gmaps is None:
        return f"Error: {_init_error}"

    query    = args.get("query")
    location = args.get("location")  # "lat,lng" string or address
    radius   = args.get("radius", 1000)
    language = args.get("language", "it")
    place_type = args.get("type")  # e.g. "train_station", "bus_station", "subway_station"

    if not query and not location:
        return "Error: Provide at least 'query' or 'location'."

    # Resolve location string to lat/lng if needed
    loc_tuple = None
    if location:
        if "," in str(location):
            parts = str(location).split(",")
            try:
                loc_tuple = (float(parts[0].strip()), float(parts[1].strip()))
            except ValueError:
                pass
        if loc_tuple is None:
            # Geocode the location string
            geo = gmaps.geocode(location, language=language)
            if geo:
                latlng = geo[0].get("geometry", {}).get("location", {})
                loc_tuple = (latlng["lat"], latlng["lng"])

    kwargs: dict[str, Any] = {"language": language}
    if query:
        kwargs["query"] = query
    if loc_tuple:
        kwargs["location"] = loc_tuple
        kwargs["radius"] = int(radius)
    if place_type:
        kwargs["type"] = place_type

    try:
        if query:
            result = gmaps.places(**kwargs)
        else:
            result = gmaps.places_nearby(**kwargs)
    except Exception as e:
        return _format_gmaps_error(e, "Places")

    places = result.get("results", [])
    if not places:
        return "No places found."

    lines = [f"Found {len(places)} place(s):"]
    for p in places[:10]:
        name     = p.get("name", "?")
        addr     = p.get("vicinity") or p.get("formatted_address", "")
        rating   = p.get("rating")
        place_id = p.get("place_id", "")
        types    = ", ".join(p.get("types", [])[:3])
        loc      = p.get("geometry", {}).get("location", {})
        lat_p    = loc.get("lat", "")
        lng_p    = loc.get("lng", "")

        line = f"• {name}"
        if addr:
            line += f"\n  Address: {addr}"
        if lat_p and lng_p:
            line += f"\n  Coords: {lat_p}, {lng_p}"
        if rating:
            line += f"\n  Rating: {rating}/5"
        if types:
            line += f"\n  Types: {types}"
        if place_id:
            line += f"\n  Place ID: {place_id}"
        lines.append(line)

    return "\n".join(lines)


def _maps_distance_matrix(args: dict) -> str:
    """Get travel time/distance between origins and destinations."""
    gmaps = _get_client()
    if gmaps is None:
        return f"Error: {_init_error}"

    origins      = args.get("origins")
    destinations = args.get("destinations")
    if not origins or not destinations:
        return "Error: Missing required parameters 'origins' and/or 'destinations'."

    if isinstance(origins, str):
        origins = [origins]
    if isinstance(destinations, str):
        destinations = [destinations]

    mode     = args.get("mode", "transit")
    language = args.get("language", "it")

    kwargs: dict[str, Any] = {
        "origins":      origins,
        "destinations": destinations,
        "mode":         mode,
        "language":     language,
    }
    if mode == "transit":
        kwargs["departure_time"] = datetime.now(timezone.utc)

    try:
        result = gmaps.distance_matrix(**kwargs)
    except Exception as e:
        return _format_gmaps_error(e, "Distance Matrix")

    rows  = result.get("rows", [])
    dest_addrs = result.get("destination_addresses", destinations)
    orig_addrs = result.get("origin_addresses", origins)

    lines = []
    for i, (row, orig) in enumerate(zip(rows, orig_addrs)):
        for j, (elem, dest) in enumerate(zip(row.get("elements", []), dest_addrs)):
            status = elem.get("status", "")
            if status == "OK":
                dur  = elem.get("duration", {}).get("text", "?")
                dist = elem.get("distance", {}).get("text", "?")
                lines.append(f"{orig}  →  {dest}")
                lines.append(f"  Duration: {dur}  |  Distance: {dist}")
            else:
                lines.append(f"{orig}  →  {dest}  [{status}]")

    return "\n".join(lines) if lines else "No results."


# ── Tool manifest ──────────────────────────────────────────────────────────────

TOOLS = [
    {
        "name": "status",
        "description": (
            "Self-check that the Google Maps integration is operational: verifies the API key is "
            "present and valid, the Geocoding API is enabled, and the network works, by performing "
            "one cheap geocode probe. Call this first whenever another Maps tool fails, or to give "
            "the user a quick yes/no on whether Maps is usable right now."
        ),
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "directions",
        "description": (
            "Get step-by-step directions from an origin to a destination. "
            "Supports transit (bus, train, metro), driving, walking, bicycling. "
            "For transit, returns detailed stop-by-stop info with departure/arrival times. "
            "Best for 'how do I get from A to B?' or 'which train do I take to go home?'."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "origin": {
                    "type": "string",
                    "description": (
                        "Starting address or place name (e.g. 'Milano Centrale') "
                        "or coordinates as 'latitude,longitude' decimal string "
                        "with no spaces (e.g. '45.4654,9.1866')."
                    ),
                },
                "destination": {
                    "type": "string",
                    "description": (
                        "Destination address or place name "
                        "or coordinates as 'latitude,longitude' decimal string "
                        "with no spaces (e.g. '45.4654,9.1866')."
                    ),
                },
                "mode": {
                    "type": "string",
                    "enum": ["transit", "driving", "walking", "bicycling"],
                    "description": "Travel mode. Default 'transit'.",
                },
                "departure_time": {
                    "type": "string",
                    "description": (
                        "When to depart. Must be the literal string 'now' (default) "
                        "or an ISO 8601 datetime string with timezone offset, "
                        "e.g. '2025-06-15T08:30:00+02:00'. "
                        "Never pass a Unix timestamp integer — always use a string."
                    ),
                },
                "transit_mode": {
                    "type": "string",
                    "enum": ["bus", "rail", "subway", "train", "tram"],
                    "description": (
                        "Restrict results to a specific transit vehicle type. "
                        "Omit to allow any vehicle. Use 'train' for intercity/regional rail, "
                        "'subway' for metro, 'tram' for tram lines, 'bus' for buses, "
                        "'rail' for any rail (train + subway + tram)."
                    ),
                },
                "transit_routing_preference": {
                    "type": "string",
                    "enum": ["less_walking", "fewer_transfers"],
                    "description": "Optimize transit route for fewer transfers or less walking.",
                },
                "alternatives": {
                    "type": "boolean",
                    "description": "Return multiple route options. Default false.",
                },
                "language": {
                    "type": "string",
                    "description": "Language for instructions. Default 'it'.",
                },
            },
            "required": ["origin", "destination"],
        },
    },
    {
        "name": "geocode",
        "description": "Convert a place name or address into geographic coordinates (latitude, longitude) and a place_id.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "address": {
                    "type": "string",
                    "description": "Address or place name to geocode.",
                },
                "language": {
                    "type": "string",
                    "description": "Language for results. Default 'it'.",
                },
                "region": {
                    "type": "string",
                    "description": "Country code bias (e.g. 'it', 'gb'). Default 'it'.",
                },
            },
            "required": ["address"],
        },
    },
    {
        "name": "reverse_geocode",
        "description": "Convert geographic coordinates (lat, lng) into a human-readable address.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "lat": {"type": "number", "description": "Latitude."},
                "lng": {"type": "number", "description": "Longitude."},
                "language": {
                    "type": "string",
                    "description": "Language for results. Default 'it'.",
                },
            },
            "required": ["lat", "lng"],
        },
    },
    {
        "name": "search_places",
        "description": (
            "Search for places near a location. "
            "Useful for finding train stations, bus stops, restaurants, etc. "
            "near an address or coordinates. "
            "At least one of 'query' or 'location' must be provided."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": (
                        "Text search query, e.g. 'stazione ferroviaria', 'bar', 'farmacia'. "
                        "Required unless 'location' is provided."
                    ),
                },
                "location": {
                    "type": "string",
                    "description": (
                        "Center of the search area: address, place name, "
                        "or 'latitude,longitude' decimal string with no spaces "
                        "(e.g. '45.4654,9.1866'). Required unless 'query' is provided."
                    ),
                },
                "radius": {
                    "type": "integer",
                    "description": "Search radius in meters. Default 1000.",
                },
                "type": {
                    "type": "string",
                    "description": (
                        "Filter by place type. Examples: 'train_station', 'bus_station', "
                        "'subway_station', 'transit_station', 'restaurant'."
                    ),
                },
                "language": {
                    "type": "string",
                    "description": "Language for results. Default 'it'.",
                },
            },
        },
    },
    {
        "name": "distance_matrix",
        "description": (
            "Calculate travel times and distances between multiple origins and destinations. "
            "Useful for comparing routes or checking ETAs."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "origins": {
                    "type": ["string", "array"],
                    "items": {"type": "string"},
                    "description": (
                        "One or more origins: address, place name, or 'latitude,longitude' "
                        "decimal string with no spaces. Pass a single string or a JSON array "
                        "of strings for multiple origins."
                    ),
                },
                "destinations": {
                    "type": ["string", "array"],
                    "items": {"type": "string"},
                    "description": (
                        "One or more destinations: address, place name, or 'latitude,longitude' "
                        "decimal string with no spaces. Pass a single string or a JSON array "
                        "of strings for multiple destinations."
                    ),
                },
                "mode": {
                    "type": "string",
                    "enum": ["transit", "driving", "walking", "bicycling"],
                    "description": "Travel mode. Default 'transit'.",
                },
                "language": {
                    "type": "string",
                    "description": "Language for results. Default 'it'.",
                },
            },
            "required": ["origins", "destinations"],
        },
    },
]


# ── JSON-RPC dispatch ──────────────────────────────────────────────────────────

TOOL_DISPATCH = {
    "status":            _maps_status,
    "directions":        _maps_directions,
    "geocode":           _maps_geocode,
    "reverse_geocode":   _maps_reverse_geocode,
    "search_places":     _maps_search_places,
    "distance_matrix":   _maps_distance_matrix,
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
                "name": "gmaps",
                "version": "1.1.0",
            },
        })

    if method == "notifications/initialized":
        return None

    if method == "tools/list":
        return _ok(req_id, {"tools": TOOLS})

    if method == "tools/call":
        params = msg.get("params", {})
        tool_name = params.get("name", "")
        tool_args = params.get("arguments", {})

        handler = TOOL_DISPATCH.get(tool_name)
        if handler is None:
            return _text_result(req_id, f"Error: Unknown tool: {tool_name}", is_error=True)

        try:
            text = handler(tool_args)
            is_err = text.startswith("Error:")
            return _text_result(req_id, text, is_error=is_err)
        except Exception as e:
            log(f"Unhandled exception in tool '{tool_name}': {e}")
            return _text_result(req_id, f"Error: Internal error in tool '{tool_name}': {e}", is_error=True)

    return json.dumps({
        "jsonrpc": "2.0",
        "id": req_id,
        "error": {"code": -32601, "message": f"Method not found: {method}"},
    })


# ── Main loop ──────────────────────────────────────────────────────────────────

def main() -> None:
    log("Starting Google Maps MCP server")
    # Eagerly initialise the client so errors surface immediately.
    _get_client()
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
