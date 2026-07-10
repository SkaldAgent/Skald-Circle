#!/usr/bin/env python3
"""
Honcho backfill script.

Deletes the existing Honcho workspace, recreates it with the correct peer
config (observe_me=true for the user peer), and re-uploads all interactive
non-ephemeral chat history from the SQLite database.

Usage:
    # Reads config from the SQLite plugins table automatically.
    python3 scripts/honcho_backfill.py

    # Or pass overrides:
    python3 scripts/honcho_backfill.py \
        --db ./database.db \
        --base-url http://localhost:8000 \
        --workspace personal-agent \
        --dry-run
"""

import argparse
import json
import sqlite3
import sys
import time
from dataclasses import dataclass
from typing import Optional

import requests


# ── Honcho API helpers ────────────────────────────────────────────────────────

class HonchoClient:
    def __init__(self, base_url: str, api_key: str = ""):
        self.base = base_url.rstrip("/")
        self.session = requests.Session()
        if api_key:
            self.session.headers["Authorization"] = f"Bearer {api_key}"
        self.session.headers["Content-Type"] = "application/json"

    def _url(self, path: str) -> str:
        return f"{self.base}{path}"

    def list_session_ids(self, workspace_id: str) -> list[str]:
        ids = []
        page = 1
        while True:
            r = self.session.post(
                self._url(f"/v3/workspaces/{workspace_id}/sessions/list"),
                params={"page": page, "size": 100},
                json={},
            )
            if r.status_code == 404:
                break
            r.raise_for_status()
            data = r.json()
            items = data.get("items", [])
            ids.extend(item["id"] for item in items)
            if page >= data.get("pages", 1):
                break
            page += 1
        return ids

    def delete_all_sessions(self, workspace_id: str):
        ids = self.list_session_ids(workspace_id)
        print(f"  deleting {len(ids)} existing session(s) …")
        for sid in ids:
            r = self.session.delete(self._url(f"/v3/workspaces/{workspace_id}/sessions/{sid}"))
            if r.status_code not in (200, 202, 204, 404):
                print(f"  WARNING: could not delete session {sid}: {r.status_code}")

    def delete_workspace(self, workspace_id: str):
        self.delete_all_sessions(workspace_id)
        r = self.session.delete(self._url(f"/v3/workspaces/{workspace_id}"))
        if r.status_code in (200, 202, 204, 404):
            print(f"  workspace '{workspace_id}' deleted (or did not exist)")
        else:
            print(f"  WARNING: DELETE workspace returned {r.status_code} — continuing anyway")

    def create_workspace(self, workspace_id: str, retries: int = 6, delay: float = 2.0):
        for attempt in range(1, retries + 1):
            r = self.session.post(self._url("/v3/workspaces"), json={"id": workspace_id})
            if r.status_code in (200, 201):
                print(f"  workspace '{workspace_id}' created")
                return
            # 409 = already exists (fine for --skip-delete path)
            if r.status_code == 409:
                print(f"  workspace '{workspace_id}' already exists — reusing")
                return
            print(f"  create workspace attempt {attempt}/{retries}: {r.status_code} — retrying in {delay}s …")
            time.sleep(delay)
        raise RuntimeError(f"POST workspace failed after {retries} attempts: {r.status_code} {r.text}")

    def create_peer(self, workspace_id: str, peer_id: str):
        r = self.session.post(
            self._url(f"/v3/workspaces/{workspace_id}/peers"),
            json={"id": peer_id},
        )
        if r.status_code in (200, 201):
            print(f"  peer '{peer_id}' created")
        elif r.status_code == 409:
            print(f"  peer '{peer_id}' already exists — reusing")
        else:
            raise RuntimeError(f"POST peer failed: {r.status_code} {r.text}")

    PEER_CONFIG = {
        "user":      {"observe_me": True},
        "assistant": {"observe_me": True},
    }

    def _add_peers(self, workspace_id: str, session_id: str):
        """Add peer config to a session via POST (separate from session creation)."""
        r = self.session.post(
            self._url(f"/v3/workspaces/{workspace_id}/sessions/{session_id}/peers"),
            json=self.PEER_CONFIG,
        )
        if r.status_code not in (200, 201, 409):
            print(f"    WARNING: add peers returned {r.status_code}: {r.text}")

    def create_session(self, workspace_id: str, session_id: str, local_id: int) -> str:
        body = {
            "id":       session_id,
            "metadata": {"local_session_id": local_id},
        }
        r = self.session.post(
            self._url(f"/v3/workspaces/{workspace_id}/sessions"),
            json=body,
        )
        if r.status_code in (200, 201):
            self._add_peers(workspace_id, session_id)
            return session_id
        if r.status_code == 409:
            print(f"    (session existed — adding peers)")
            self._add_peers(workspace_id, session_id)
            return session_id
        raise RuntimeError(f"POST session failed: {r.status_code} {r.text}")

    def fix_all_session_peers(self, workspace_id: str):
        """Add correct peer config to all existing sessions in the workspace."""
        ids = self.list_session_ids(workspace_id)
        print(f"Fixing peers on {len(ids)} session(s) …")
        for sid in ids:
            self._add_peers(workspace_id, sid)
            print(f"  {sid}", end="\r")
        print(f"\nDone — {len(ids)} session(s) updated.")

    def add_message(
        self,
        workspace_id: str,
        session_id: str,
        peer_id: str,
        content: str,
        local_message_id: int,
        created_at: str,
    ):
        body = {
            "messages": [
                {
                    "peer_id":    peer_id,
                    "content":    content,
                    "metadata":   {"local_message_id": local_message_id},
                    "created_at": created_at,
                }
            ]
        }
        r = self.session.post(
            self._url(f"/v3/workspaces/{workspace_id}/sessions/{session_id}/messages"),
            json=body,
        )
        if r.status_code not in (200, 201, 409):
            raise RuntimeError(
                f"POST message failed (session={session_id}): {r.status_code} {r.text}"
            )


# ── DB helpers ────────────────────────────────────────────────────────────────

@dataclass
class Session:
    id: int
    source: str

@dataclass
class Message:
    id: int
    role: str
    content: str
    created_at: str


def load_plugin_config(db_path: str) -> Optional[dict]:
    """Read honcho plugin config from the plugins table."""
    try:
        con = sqlite3.connect(db_path)
        row = con.execute(
            "SELECT enabled, config FROM plugins WHERE id = 'honcho'"
        ).fetchone()
        con.close()
        if row is None:
            return None
        enabled, config_json = row
        if not enabled:
            print("WARNING: honcho plugin is disabled in DB; proceeding anyway")
        return json.loads(config_json)
    except Exception as e:
        print(f"WARNING: could not read plugin config from DB: {e}")
        return None


def load_sessions(db_path: str) -> list[Session]:
    con = sqlite3.connect(db_path)
    rows = con.execute(
        """
        SELECT id, source
        FROM   chat_sessions
        WHERE  is_interactive = 1
          AND  is_ephemeral   = 0
          AND  source NOT IN ('tic', 'cron')
        ORDER  BY id
        """
    ).fetchall()
    con.close()
    return [Session(id=r[0], source=r[1]) for r in rows]


def load_messages(db_path: str, session_id: int) -> list[Message]:
    """
    Load all user/assistant messages for a session, ordered chronologically.
    Excludes: sub-agent messages (role='agent'), failed, synthetic, empty.
    """
    con = sqlite3.connect(db_path)
    rows = con.execute(
        """
        SELECT h.id, h.role, h.content, h.created_at
        FROM   chat_history          h
        JOIN   chat_sessions_stack   s ON s.id = h.session_stack_id
        WHERE  s.session_id   = ?
          AND  h.role        IN ('user', 'assistant')
          AND  h.status       = 'ok'
          AND  h.is_synthetic = 0
          AND  h.content     != ''
        ORDER  BY h.id
        """,
        (session_id,),
    ).fetchall()
    con.close()
    return [Message(id=r[0], role=r[1], content=r[2], created_at=r[3]) for r in rows]


# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="Backfill Honcho from local SQLite DB")
    parser.add_argument("--db",          default="./database.db",  help="Path to SQLite DB")
    parser.add_argument("--base-url",    default=None,             help="Honcho base URL (overrides DB config)")
    parser.add_argument("--workspace",   default=None,             help="Honcho workspace ID (overrides DB config)")
    parser.add_argument("--api-key",     default="",               help="Honcho API key")
    parser.add_argument("--dry-run",     action="store_true",      help="Print plan without touching Honcho")
    parser.add_argument("--delay-ms",    type=int, default=50,     help="Delay between message uploads (ms)")
    parser.add_argument("--skip-delete", action="store_true",      help="Skip workspace deletion (add to existing)")
    parser.add_argument("--fix-peers",   action="store_true",      help="Only fix peer config on existing sessions, then exit")
    args = parser.parse_args()

    # ── Resolve config ────────────────────────────────────────────────────────
    plugin_cfg   = load_plugin_config(args.db)
    base_url     = args.base_url  or (plugin_cfg or {}).get("base_url",     "http://localhost:8000")
    workspace_id = args.workspace or (plugin_cfg or {}).get("workspace_id", "personal-agent")
    api_key      = args.api_key   or (plugin_cfg or {}).get("api_key",      "")

    print(f"Honcho base URL : {base_url}")
    print(f"Workspace ID    : {workspace_id}")
    print(f"DB              : {args.db}")
    print()

    client = HonchoClient(base_url, api_key)

    # ── Fix-peers only mode ───────────────────────────────────────────────────
    if args.fix_peers:
        client.fix_all_session_peers(workspace_id)
        return

    # ── Load sessions ─────────────────────────────────────────────────────────
    sessions = load_sessions(args.db)
    print(f"Found {len(sessions)} interactive non-ephemeral session(s)")

    total_msgs = 0
    plan = []
    for sess in sessions:
        msgs = load_messages(args.db, sess.id)
        if not msgs:
            continue
        honcho_id = f"{workspace_id}-{sess.id}"
        plan.append((sess, msgs, honcho_id))
        total_msgs += len(msgs)
        print(f"  session {sess.id:4d}  ({sess.source:10s})  {len(msgs):4d} msgs  →  {honcho_id}")

    print(f"\nTotal messages to upload: {total_msgs}")

    if args.dry_run:
        print("\n[dry-run] No changes made.")
        return

    if not plan:
        print("Nothing to upload.")
        return

    confirm = input("\nProceed? This will DELETE and recreate the Honcho workspace. [y/N] ")
    if confirm.strip().lower() != "y":
        print("Aborted.")
        sys.exit(0)

    delay_s = args.delay_ms / 1000.0

    # ── Reset workspace ───────────────────────────────────────────────────────
    if not args.skip_delete:
        print("\n[1/3] Deleting existing workspace …")
        client.delete_workspace(workspace_id)
        time.sleep(1)

    print("\n[2/3] Creating workspace and peers …")
    client.create_workspace(workspace_id)
    client.create_peer(workspace_id, "user")
    client.create_peer(workspace_id, "assistant")

    # ── Upload messages ───────────────────────────────────────────────────────
    print(f"\n[3/3] Uploading {total_msgs} messages …")

    for sess, msgs, honcho_id in plan:
        print(f"\n  session {sess.id} → {honcho_id}  ({len(msgs)} messages)")
        client.create_session(workspace_id, honcho_id, sess.id)

        for i, msg in enumerate(msgs, 1):
            peer_id = "user" if msg.role == "user" else "assistant"
            try:
                client.add_message(
                    workspace_id=workspace_id,
                    session_id=honcho_id,
                    peer_id=peer_id,
                    content=msg.content,
                    local_message_id=msg.id,
                    created_at=msg.created_at,
                )
                print(f"    [{i:4d}/{len(msgs)}] {peer_id:9s} id={msg.id}", end="\r")
            except RuntimeError as e:
                print(f"\n    ERROR on msg {msg.id}: {e} — skipping")

            if delay_s > 0:
                time.sleep(delay_s)

        print(f"    [{len(msgs):4d}/{len(msgs)}] done                         ")

    print("\nBackfill complete.")
    print("Honcho deriver will process messages in the background.")
    print("Restart personal-agent to reconnect the plugin.")


if __name__ == "__main__":
    main()
