#!/usr/bin/env python3
"""
Inspect the last N llm_requests rows for a given model (default: deepseek).
Prints a structured summary without dumping raw payloads.

Usage:
    python scripts/inspect_llm_requests.py [model_filter] [rows]

Examples:
    python scripts/inspect_llm_requests.py deepseek 5
    python scripts/inspect_llm_requests.py anthropic 3
"""

import json
import sqlite3
import sys
from pathlib import Path

DB_PATH = Path(__file__).parent.parent / "database.db"
MODEL_FILTER = sys.argv[1] if len(sys.argv) > 1 else "deepseek"
ROWS = int(sys.argv[2]) if len(sys.argv) > 2 else 5


def fmt_len(s):
    if s is None:
        return "null"
    return f"{len(s)} chars"


def summarize_message(i, msg):
    role = msg.get("role", "?")
    content = msg.get("content")
    tool_calls = msg.get("tool_calls")
    tool_call_id = msg.get("tool_call_id")
    reasoning = msg.get("reasoning_content")

    parts = []

    if isinstance(content, str):
        parts.append(f"{len(content)} chars")
    elif isinstance(content, list):
        total = sum(len(b.get("text", "")) for b in content if isinstance(b, dict))
        cache_tags = [b for b in content if isinstance(b, dict) and "cache_control" in b]
        parts.append(f"{total} chars (content array, {len(content)} blocks)")
        if cache_tags:
            parts.append(f"[cache_control on {len(cache_tags)} block(s)]")
    elif content is None:
        parts.append("(no content)")

    if reasoning:
        parts.append(f"[reasoning_content: {len(reasoning)} chars]")

    if tool_calls:
        names = [tc.get("function", {}).get("name", "?") for tc in tool_calls]
        parts.append(f"[tool_calls: {', '.join(names)}]")

    if tool_call_id:
        parts.append(f"(tool_call_id={tool_call_id})")

    detail = "  ".join(parts)
    print(f"  {i:>3}  {role:<12} {detail}")


def est_tokens(obj) -> int:
    """Rough token estimate: serialized chars / 4."""
    return len(json.dumps(obj)) // 4


def summarize_request(row):
    rid, model_name, req_json, req_headers, resp_json, input_tok, output_tok, duration_ms, created_at = row

    print(f"\n{'='*70}")
    print(f"id={rid}  model={model_name}  created={created_at}")
    print(f"tokens: input={input_tok}  output={output_tok}  duration={duration_ms}ms")

    try:
        req = json.loads(req_json) if req_json else {}
    except Exception as e:
        print(f"  [ERROR parsing request_json: {e}]")
        return

    # Top-level params (excluding messages and tools)
    skip = {"messages", "tools", "model"}
    params = {k: v for k, v in req.items() if k not in skip}
    if params:
        print(f"\n[params]")
        for k, v in params.items():
            print(f"  {k} = {json.dumps(v)}")

    # Tools
    tools = req.get("tools", [])
    if tools:
        tool_names = [t.get("function", {}).get("name", "?") for t in tools]
        tools_tok = est_tokens(tools)
        print(f"\n[tools]  {len(tools)} defined  ~{tools_tok} tok est")
        print(f"  {', '.join(tool_names)}")
        last = tools[-1]
        if "cache_control" in last:
            print(f"  last tool has cache_control: {last['cache_control']}")

    # Messages
    messages = req.get("messages", [])
    sys_msgs  = [m for m in messages if m.get("role") == "system"]
    sys_tok   = est_tokens(sys_msgs)
    conv_msgs = [m for m in messages if m.get("role") != "system"]
    conv_tok  = est_tokens(conv_msgs)
    print(f"\n[messages]  {len(messages)} total  (~{est_tokens(messages)} tok est: {len(sys_msgs)} system ~{sys_tok} tok, {len(conv_msgs)} conv ~{conv_tok} tok)")
    for i, msg in enumerate(messages):
        summarize_message(i, msg)

    # Response summary
    if resp_json:
        try:
            resp = json.loads(resp_json)
            usage = resp.get("usage", {})
            if usage:
                print(f"\n[usage]")
                for k, v in usage.items():
                    print(f"  {k} = {v}")
        except Exception:
            pass


def main():
    conn = sqlite3.connect(DB_PATH)
    rows = conn.execute(
        """
        SELECT id, model_name, request_json, request_headers,
               response_json, input_tokens, output_tokens, duration_ms, created_at
        FROM   llm_requests
        WHERE  model_name LIKE ?
        ORDER  BY id DESC
        LIMIT  ?
        """,
        (f"%{MODEL_FILTER}%", ROWS),
    ).fetchall()
    conn.close()

    if not rows:
        print(f"No rows found for model filter '{MODEL_FILTER}'")
        return

    print(f"Last {len(rows)} request(s) matching '{MODEL_FILTER}' (newest first)")
    for row in rows:
        summarize_request(row)
    print(f"\n{'='*70}")


if __name__ == "__main__":
    main()
