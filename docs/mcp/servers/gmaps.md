# Google Maps MCP Server (gmaps)

## Overview

A Python MCP server providing **public-transit & mapping** capabilities via the Google Maps Platform APIs.

**Server name:** `gmaps`  
**Transport:** `stdio` (spawns `python3 scripts/gmaps_mcp_server.py`)  
**Location:** `scripts/gmaps_mcp_server.py`

---

## Tools

All tools are callable as `mcp__gmaps__<tool>`; the table lists the bare `<tool>` names.

| Tool | Required params | Optional params | Description |
|------|-----------------|-----------------|-------------|
| `status` | *(none)* | *(none)* | Self-check: verifies the API key is valid and the Geocoding API responds. Call first when another Maps tool fails. |
| `directions` | `origin`, `destination` | `mode`, `departure_time`, `transit_mode`, `transit_routing_preference`, `alternatives`, `language` | Step-by-step directions (transit, driving, walking, bicycling) |
| `geocode` | `address` | `language`, `region` | Address / place name → coordinates + place_id |
| `reverse_geocode` | `lat`, `lng` | `language` | Coordinates → formatted address |
| `search_places` | *(at least one of `query`/`location`)* | `radius`, `type`, `language` | Find nearby stations, stops, POIs |
| `distance_matrix` | `origins`, `destinations` | `mode`, `language` | Travel time & distance between multiple points |

---

## Authentication

### API Key

Unlike Gmail/Calendar (OAuth), Google Maps uses a **plain API key**.

**Priority order:**

1. Environment variable `GOOGLE_MAPS_API_KEY`
2. File `secrets/gmaps_api_key.txt` (first non-empty line)

The `secrets/` directory is in `.gitignore` — the key will not be committed.

### Required Google Cloud APIs

Enable all four in the [Google Cloud Console](https://console.cloud.google.com/apis/library):

| API | Used by |
|-----|---------|
| **Directions API** | `directions` |
| **Geocoding API** | `geocode`, `reverse_geocode` |
| **Places API** | `search_places` |
| **Distance Matrix API** | `distance_matrix` |

---

## Setup

### 1. Create API Key

1. Go to [Google Cloud Console → Credentials](https://console.cloud.google.com/apis/credentials)
2. Click **Create credentials → API key**
3. (Recommended) Restrict the key to the four APIs above

### 2. Save the key

```bash
echo "YOUR_API_KEY_HERE" > secrets/gmaps_api_key.txt
```

Or set the environment variable in your shell/`run.sh`:

```bash
export GOOGLE_MAPS_API_KEY=AIza...
```

### 3. Install the Python dependency

```bash
.venv/bin/pip install googlemaps
# or, if you re-run run.sh, it installs requirements.txt automatically
```

`googlemaps` is already listed in `requirements.txt`.

### 4. Register the server with the agent

Ask the agent:
```
register_mcp(
  name="gmaps",
  transport="stdio",
  command="python3",
  args=["scripts/gmaps_mcp_server.py"]
)
```

---

## Usage Examples

### Self-check: is Maps working?

```
mcp__gmaps__status()
```
Returns `OK: …` if the API key is present, valid, and the Geocoding API responds; otherwise an `Error:` string explaining what to fix. Call this first whenever another Maps tool fails.

### Transit directions home (now)

```
mcp__gmaps__directions(
  origin="Piazza del Duomo, Milano",
  destination="casa mia",   ← or the real address saved in agent memory
  mode="transit"
)
```

### Prefer train, fewer transfers

```
mcp__gmaps__directions(
  origin="current location",
  destination="Via Roma 1, Torino",
  mode="transit",
  transit_mode="train",
  transit_routing_preference="fewer_transfers"
)
```

### Find the nearest metro station

```
mcp__gmaps__search_places(
  query="metro",
  location="Piazza Garibaldi, Napoli",
  radius=500,
  type="subway_station"
)
```

### How long does it take from A to B?

```
mcp__gmaps__distance_matrix(
  origins="Stazione Centrale, Milano",
  destinations="Aeroporto Malpensa",
  mode="transit"
)
```

---

## Enable / Disable

```
toggle_item(kind="mcp", id="gmaps", enabled=false)   # disable
toggle_item(kind="mcp", id="gmaps", enabled=true)    # re-enable
restart                                    # required for changes to take effect
```

---

## Dependencies

| Package | Version | Purpose |
|---------|---------|---------|
| `googlemaps` | latest | Google Maps Platform Python client |

---

## Parameter notes

### Coordinates format

Whenever a parameter accepts coordinates, pass them as a **`"latitude,longitude"` decimal string with no spaces**, e.g. `"45.4654,9.1866"`. Never pass an array or separate fields.

### `departure_time`

Must be the **literal string `"now"`** or an **ISO 8601 datetime string with timezone offset**, e.g. `"2025-06-15T08:30:00+02:00"`. Never pass a Unix timestamp integer — the tool now rejects non-string values with an explicit error.

### `transit_mode`

Restricts results to a specific vehicle: `"train"` = intercity/regional rail, `"subway"` = metro, `"tram"` = trams, `"bus"` = buses, `"rail"` = any rail. Omit to allow any vehicle.

---

## Error Handling

| Error | Response |
|-------|----------|
| API key not found | `"Error: Google Maps API key not found. Set GOOGLE_MAPS_API_KEY…"` |
| `googlemaps` not installed | `"Error: Missing dependency: No module named 'googlemaps'. Run: pip install googlemaps"` |
| `OVER_QUERY_LIMIT` | `"Error: <API> API quota exceeded (OVER_QUERY_LIMIT). Check usage and billing in the Google Cloud Console."` |
| `REQUEST_DENIED` | `"Error: <API> API request denied (REQUEST_DENIED). Verify that the API key … is valid and that the <API> API is enabled …"` |
| `INVALID_REQUEST` | `"Error: <API> API rejected the request as invalid (INVALID_REQUEST). Check that the addresses, coordinates, and parameters are well-formed."` |
| `MAX_ELEMENTS_EXCEEDED` | `"Error: <API> API returned MAX_ELEMENTS_EXCEEDED — too many origins×destinations at once. …"` |
| `NOT_FOUND` | `"Error: <API> API could not geocode one of the supplied places. …"` |
| Timeout | `"Error: <API> API request timed out. Retry in a moment."` |
| No route found | `"No routes found from '…' to '…'."` |
| Missing required param | `"Error: Missing required parameter '…'"` |
| Invalid `departure_time` | `"Error: 'departure_time' must be 'now' or an ISO 8601 string …"` |

All error responses are flagged with `isError: true`. The error label `<API>` reflects which Google Maps Platform API was being called (Directions, Geocoding, Places, Distance Matrix).

All errors are logged to stderr with `[gmaps_mcp]` prefix.

---

## Protocol

Implements JSON-RPC 2.0 over stdio (same as gcal and gmail servers):

- **Requests:** read from stdin, one JSON object per line
- **Responses:** written to stdout
- **Logs:** stderr only, prefixed `[gmaps_mcp]`

Supported methods: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`.

---

## When to Update This File

- New tools added
- Auth mechanism changes (e.g. OAuth migration)
- New transport option added
- Error cases change
