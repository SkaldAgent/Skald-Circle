# MCP servers

MCP tools are lazy-loaded. The system prompt shows available servers — call `activate_tools(["name", ...])` to load their tools into the session. The grant persists for the whole session (survives restart). You do not need to call it again for the same server.

Once active, tools are called as `mcp__<server>__<tool>` (e.g. `mcp__gmail__send_message`, `mcp__gcal__list_events`).

<!-- MCP_LIST -->
