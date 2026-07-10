#!/usr/bin/env python3
"""Demo MCP server (stdio, JSON-RPC 2.0) exercising MCP elicitation.

Two tools demonstrate the two card types Skald renders in the Agent Inbox:

  * ``ask_secret``   — elicits a single masked field (``format: password``).
                       Returns only a masked confirmation; the value is held in
                       RAM (this process) and never echoed back to the caller.
  * ``confirm``      — elicits with an empty schema → a yes/no confirmation.

Register it from the LLM ("register an MCP server, command python3, args
scripts/elicitation_demo_mcp.py") or via the MCP servers UI, then ask the agent
to call the tool. The request appears in the Agent Inbox under "Secrets".

No third-party dependencies: a plain ``readline`` JSON-RPC loop, matching how
Skald's stdio client speaks. ``elicitation/create`` is a server→client request;
the reply arrives on the same stdin and is matched by its id.
"""

import sys
import json
import itertools

_next_id = itertools.count(1)
# Demo-only in-RAM secret cache, mirroring the SSH MCP "prompt" method: keep the
# value for the process lifetime, never write it to disk, never return it.
_secret_cache: dict[str, str] = {}


def send(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def readline() -> dict | None:
    """Blocking read of one non-empty JSON-RPC line; None on EOF."""
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        line = line.strip()
        if line:
            return json.loads(line)


def elicit(message: str, requested_schema: dict) -> dict:
    """Send ``elicitation/create`` and block until the matching reply arrives.

    Returns the JSON-RPC ``result`` ({"action": ..., "content": {...}}).
    """
    eid = f"elicit-{next(_next_id)}"
    send({
        "jsonrpc": "2.0",
        "id": eid,
        "method": "elicitation/create",
        "params": {"message": message, "requestedSchema": requested_schema},
    })
    while True:
        msg = readline()
        if msg is None:
            return {"action": "cancel"}
        if msg.get("id") == eid:
            return msg.get("result", {"action": "cancel"})
        # Any other inbound message mid-wait is ignored for this demo.


TOOLS = [
    {
        "name": "ask_secret",
        "description": "Ask the user for a secret value (masked) via elicitation.",
        "inputSchema": {
            "type": "object",
            "properties": {"label": {"type": "string", "description": "what the secret is for"}},
        },
    },
    {
        "name": "confirm",
        "description": "Ask the user to confirm an action (yes/no) via elicitation.",
        "inputSchema": {
            "type": "object",
            "properties": {"action": {"type": "string", "description": "the action to confirm"}},
        },
    },
]


def text_result(mid, text: str, is_error: bool = False) -> None:
    send({"jsonrpc": "2.0", "id": mid, "result": {
        "content": [{"type": "text", "text": text}], "isError": is_error}})


def handle_call(mid, name: str, args: dict) -> None:
    if name == "ask_secret":
        label = args.get("label", "the secret")
        result = elicit(
            f"Enter {label}",
            {"type": "object",
             "properties": {"secret": {"type": "string", "format": "password",
                                       "title": label}},
             "required": ["secret"]},
        )
        action = result.get("action")
        if action == "accept":
            value = (result.get("content") or {}).get("secret", "")
            _secret_cache[label] = value
            # Never return the secret itself — only proof we received it.
            text_result(mid, f"OK — received {label} ({len(value)} chars, kept in RAM).")
        else:
            text_result(mid, f"Error: {label} required (user {action}).", is_error=True)

    elif name == "confirm":
        what = args.get("action", "this action")
        result = elicit(f"Confirm: {what}?", {"type": "object", "properties": {}})
        action = result.get("action")
        text_result(mid, f"User {action}ed: {what}." if action == "accept"
                    else f"Not confirmed ({action}): {what}.",
                    is_error=(action != "accept"))
    else:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32602, "message": f"unknown tool: {name}"}})


def main() -> None:
    while True:
        msg = readline()
        if msg is None:
            break
        mid = msg.get("id")
        method = msg.get("method")
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": mid, "result": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "serverInfo": {"name": "elicitation-demo", "version": "0.1.0"}}})
        elif method == "notifications/initialized":
            pass
        elif method == "tools/list":
            send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
        elif method == "tools/call":
            params = msg.get("params", {})
            handle_call(mid, params.get("name", ""), params.get("arguments", {}) or {})
        elif mid is not None:
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32601, "message": f"method not found: {method}"}})


if __name__ == "__main__":
    main()
