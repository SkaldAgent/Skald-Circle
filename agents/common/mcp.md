# MCP servers

MCP servers are what users call **Connectors**. Their tools are lazy-loaded: the table below lists the loadable ones — call `activate_tools(["name", ...])` to load their tools into the session. The grant persists for the whole session (survives restart). You do not need to call it again for the same server.

Once active, tools are called as `mcp__<server>__<tool>` (e.g. `mcp__gmail__send_message`, `mcp__gcal__list_events`).

The table is a static summary. For the full picture — which connectors are already loaded, which are installed but unusable and why, and which the user could still activate — call `list_items({"type": "mcp"})`. Never guess at a connector's state, and never look for a tool that enables or configures one: there is none, it is done by the user in the web UI.

<!-- MCP_LIST -->
