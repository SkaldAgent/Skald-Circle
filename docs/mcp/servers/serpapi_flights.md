# SerpAPI Google Flights MCP Server (serpapi_flights)

## Overview

A Python MCP server providing **flight search** capabilities via the SerpAPI Google Flights engine.

**Server name:** `serpapi_flights`  
**Transport:** `stdio` (spawns `python3 scripts/mcp/serpapi_flights/server.py`)  
**Location:** `scripts/mcp/serpapi_flights/server.py`

---

## Tools

| Tool | Required params | Optional params | Description |
|------|-----------------|-----------------|-------------|
| `serpapi_search_flights` | `departure_id`, `arrival_id`, `outbound_date` | `return_date`, `adults`, `children`, `infants_in_seat`, `infants_on_lap`, `stops`, `currency`, `preferred_cabins`, `hl`, `max_results` | One-way or round-trip flight search on Google Flights |

The previous `serpapi_lookup_airport` tool (hardcoded dictionary of ~100 airports) was removed: the LLM already knows the common IATA codes, and SerpAPI itself accepts both airport codes (`JFK`, `FCO`) and city codes covering all airports of a city (`NYC`, `ROM`, `MIL`, `LON`).

---

## Authentication

### API key

SerpAPI uses a single API key (no OAuth).

**Priority order:**

1. Environment variable `SERPAPI_API_KEY`
2. File `secrets/serpapi_api_key.txt` (first non-empty line)

The `secrets/` directory is in `.gitignore` — the key will not be committed.

### Get a key

1. Sign up at [serpapi.com](https://serpapi.com).
2. Copy your API key from the dashboard.

### Save the key

```bash
echo "YOUR_SERPAPI_KEY_HERE" > secrets/serpapi_api_key.txt
```

Or set the environment variable:

```bash
export SERPAPI_API_KEY=your_key_here
```

---

## Setup

### 1. Install the Python dependency

`httpx` is listed in the project root `requirements.txt` and is installed automatically by `run.sh` into `.venv/`. To install manually:

```bash
uv pip install httpx
# or: .venv/bin/pip install httpx
```

The local `scripts/mcp/serpapi_flights/requirements.txt` is kept for standalone use and contains only `httpx>=0.27.0`.

### 2. Register the server with the agent

Ask the agent:

```
register_mcp(
  name="serpapi_flights",
  transport="stdio",
  command="python3",
  args=["scripts/mcp/serpapi_flights/server.py"]
)
```

---

## Usage Examples

### One-way, cheapest Milan → Rome next week

```
mcp__serpapi_flights__serpapi_search_flights(
  departure_id="MIL",
  arrival_id="ROM",
  outbound_date="2026-07-01"
)
```

### Round-trip London → New York, 2 adults, business class, non-stop only

```
mcp__serpapi_flights__serpapi_search_flights(
  departure_id="LON",
  arrival_id="NYC",
  outbound_date="2026-08-01",
  return_date="2026-08-15",
  adults=2,
  preferred_cabins="business",
  stops=0,
  currency="USD"
)
```

---

## Parameter notes

### Airport vs city codes

`departure_id` and `arrival_id` must be **exactly 3 ASCII letters**, uppercased automatically. Both forms are accepted:

- **Airport code** — a specific airport: `JFK`, `FCO` (Rome Fiumicino), `LGW` (London Gatwick).
- **City code** — covers all airports of a city: `NYC`, `ROM`, `MIL`, `LON`. **Prefer city codes** when the user does not name a specific airport.

### `stops`

Integer enum: `0` = non-stop only, `1` = max one stop, `2` = max two stops. Omit to allow any.

### `type` (handled automatically)

The server sends SerpAPI `type=2` for one-way (no `return_date`) and `type=1` for round-trip. The caller never sets `type` directly.

### `currency`

ISO 4217 code (3 uppercase letters). Default `EUR`.

### `hl`

Language code for SerpAPI result text (e.g. `"en"`, `"it"`). Default `"en"`.

---

## Error Handling

| Error | Response |
|-------|----------|
| API key missing | `"Error: SerpAPI API key not found. Set SERPAPI_API_KEY env var or create secrets/serpapi_api_key.txt …"` |
| HTTP 401 | `"Error: Invalid SerpAPI API key. Check secrets/serpapi_api_key.txt or the SERPAPI_API_KEY env var."` |
| HTTP 429 | `"Error: SerpAPI rate limit exceeded. Wait a moment and retry."` |
| HTTP 400 | `"Error: Bad request — <body>. Verify airport codes (3-letter IATA) and dates."` |
| Timeout (30s) | `"Error: Request to SerpAPI timed out (30s). …"` |
| Missing/invalid param | `"Error: 'outbound_date' must be in YYYY-MM-DD format (got '…')."` |
| Unknown tool | `"Error: Unknown tool: <name>"` |
| SerpAPI error in body | `"Error: SerpAPI returned an error: <message>"` |

Every error is returned as a tool result with `isError: true` (detected by the `Error:` prefix).

All errors are logged to stderr with `[serpapi_flights_mcp]` prefix.

---

## Protocol

Implements JSON-RPC 2.0 over stdio (same shape as the `gmaps`, `gcal`, `gmail`, and `whatsapp` servers):

- **Requests:** read from stdin, one JSON object per line
- **Responses:** written to stdout
- **Logs:** stderr only, prefixed `[serpapi_flights_mcp]`

Supported methods: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`.

The server is fully synchronous: stdin is read line-by-line, each `tools/call` performs a blocking `httpx.Client.get` to SerpAPI and returns the formatted result. No background threads, no FastMCP framework.

---

## When to Update This File

- New tools added
- Auth mechanism changes
- SerpAPI parameters added or removed
- Error cases change
