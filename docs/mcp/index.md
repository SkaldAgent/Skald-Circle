# MCP (Model Context Protocol)

External tool integration via Model Context Protocol servers.

## Files

- [mcp.md](../mcp.md) — McpManager, transports (stdio/HTTP), protocol version + `MCP-Protocol-Version` header, `tools/list` pagination, structured tool results, media tool results (image/audio/resource → `data/mcp_media/` + `/api/mcp-media/`), cancellation (`notifications/cancelled`) + Tasks (block-and-poll `tasks/get`/`result`/`cancel`), enable/disable, tool registration
- **Specification reference** (in `specs/` subdirectory) — one file per official MCP spec revision, as background for future Skald MCP work. See [specs/index.md](specs/index.md) for the comparative overview:
  - [specs/2026-07-28-draft.md](specs/2026-07-28-draft.md) — Draft / RC: stateless redesign, per-request negotiation, extensions framework
  - [specs/2025-11-25.md](specs/2025-11-25.md) — Latest Stable: OIDC Discovery, icons, URL-mode elicitation, experimental Tasks
  - [specs/2025-06-18.md](specs/2025-06-18.md) — Stable: Streamable HTTP, Elicitation, structured tool output
  - [specs/2024-11-05.md](specs/2024-11-05.md) — Legacy: first public release
- **Servers** (in `servers/` subdirectory):
  - [servers/gmail.md](servers/gmail.md) — Gmail read+modify+send MCP server (custom Python)
  - [servers/gcal.md](servers/gcal.md) — Google Calendar read+write MCP server (custom Python)
  - [servers/gmaps.md](servers/gmaps.md) — Google Maps transit/directions MCP server (custom Python)
  - [servers/serpapi_flights.md](servers/serpapi_flights.md) — SerpAPI Google Flights search MCP server (custom Python)
  - [servers/whatsapp.md](servers/whatsapp.md) — WhatsApp read+send MCP server (custom Node.js)

See [../index.md#mcp-model-context-protocol](../index.md#mcp-model-context-protocol) for navigation.
