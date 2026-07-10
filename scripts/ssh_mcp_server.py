#!/usr/bin/env python3
"""SSH MCP server (JSON-RPC 2.0 over stdio).

Exposes SSH tools that operate on remote hosts with **the same output format**
as Skald's native filesystem tools (`read_file`, `list_files`, `grep_files`,
`edit_file`, `replace_lines`, `exec`). The only thing the LLM sees differently
is the first `alias` argument selecting the host. Tool names here are bare
(`read_file`, `exec`, …); Skald prepends the `mcp__ssh__` prefix automatically.

Hosts are addressed by alias — hostname and credentials never appear in tool
calls. Aliases live in ``secrets/ssh_aliases.json`` (auto-managed, never edited
by hand). No secret is ever stored in that file.

Login auth (``auth`` per alias, set on ``add_alias``):
  * ``key``      — SSH key / ssh-agent only (default). If the chosen private key
    is encrypted, its passphrase is requested on demand via **MCP elicitation**
    (lazy: only when paramiko reports the key needs one). ``SSH_MCP_KEY_PASSPHRASE``
    still works as a non-interactive override.
  * ``password`` — login password requested on demand via **MCP elicitation**
    (Skald shows a masked field in the Agent Inbox); agent/key auth is skipped.

Elicited login secrets are kept only in this process's RAM with a short TTL
(``SSH_MCP_LOGIN_PW_TTL``), never sent to the LLM and never written to disk;
they are dropped on an authentication failure so the next attempt re-prompts.

sudo (two methods per alias, set on ``add_alias``):
  * ``nopasswd`` — ``sudo -n``: non-interactive, fails fast if NOPASSWD is not
    configured on the host (no hung channel). No secret stored anywhere.
  * ``prompt``  — ``sudo -S``: the password is requested on demand via **MCP
    elicitation** (Skald shows a masked field in the Agent Inbox), fed to
    sudo's stdin, kept only in this process's RAM with a short TTL, never sent
    to the LLM and never written to disk.

Connections are pooled per alias with lazy TTL eviction. Host keys are verified
against ``~/.ssh/known_hosts`` (unknown hosts are rejected unless the alias was
added with ``accept_new_host_key=true``).

Run with:
  python3 scripts/ssh_mcp_server.py

Dependency: paramiko>=3.4 (in requirements.txt; installed into .venv by run.sh).
"""

from __future__ import annotations

import itertools
import json
import os
import posixpath
import re
import shlex
import socket
import stat
import sys
import time
from typing import Any


# ── Config ───────────────────────────────────────────────────────────────────

_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ALIASES_FILE = os.path.join(_ROOT, "secrets", "ssh_aliases.json")

POOL_TTL = int(os.environ.get("SSH_MCP_POOL_TTL", "300"))            # idle connection eviction
SUDO_PW_TTL = int(os.environ.get("SSH_MCP_SUDO_PW_TTL", "300"))      # in-RAM sudo password cache
LOGIN_PW_TTL = int(os.environ.get("SSH_MCP_LOGIN_PW_TTL", "300"))    # in-RAM login/passphrase cache
CONNECT_TIMEOUT = int(os.environ.get("SSH_MCP_CONNECT_TIMEOUT", "15"))
DEFAULT_CMD_TIMEOUT = int(os.environ.get("SSH_MCP_COMMAND_TIMEOUT", "120"))

# Mirror the native list_files skip set so remote listings match local ones.
SKIP_DIRS = {"target", ".git", "node_modules", ".venv", "__pycache__", "secrets"}

# Match the native read_file cap.
MAX_READ_LINES = 2000


def log(msg: str) -> None:
    """Log to stderr; stdout is reserved for JSON-RPC."""
    print(f"[ssh_mcp] {msg}", file=sys.stderr, flush=True)


class ToolError(Exception):
    """Expected, user-facing failure. Surfaced as ``Error: <message>``."""


# ── stdio JSON-RPC I/O (single readline path so elicit() can re-enter) ─────────

def send(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def readline() -> dict | None:
    """Blocking read of one non-empty JSON-RPC message; None on EOF."""
    while True:
        line = sys.stdin.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            continue
        try:
            return json.loads(line)
        except json.JSONDecodeError as e:
            log(f"invalid JSON input: {e}")
            continue


_eid = itertools.count(1)


def elicit(message: str, requested_schema: dict) -> dict:
    """Send an ``elicitation/create`` request and block until the reply arrives.

    Returns the JSON-RPC ``result`` ({"action": ..., "content": {...}}). While
    waiting, any other inbound message is ignored (v1: serial processing).
    """
    eid = f"ssh-elicit-{next(_eid)}"
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
        log(f"ignoring inbound while awaiting elicitation: {msg.get('method') or msg.get('id')}")


def _ok(req_id: Any, result: Any) -> dict:
    return {"jsonrpc": "2.0", "id": req_id, "result": result}


def _text_result(req_id: Any, text: str, is_error: bool = False) -> dict:
    res: dict = {"content": [{"type": "text", "text": text}]}
    if is_error:
        res["isError"] = True
    return {"jsonrpc": "2.0", "id": req_id, "result": res}


# ── Alias store (auto-managed, 0600) ───────────────────────────────────────────

def _load_aliases() -> dict:
    try:
        with open(ALIASES_FILE) as f:
            return json.load(f)
    except FileNotFoundError:
        return {"aliases": []}
    except Exception as e:
        log(f"failed to read aliases: {e}")
        return {"aliases": []}


def _save_aliases(data: dict) -> None:
    os.makedirs(os.path.dirname(ALIASES_FILE), exist_ok=True)
    tmp = f"{ALIASES_FILE}.tmp.{os.getpid()}"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=2)
    os.replace(tmp, ALIASES_FILE)
    try:
        os.chmod(ALIASES_FILE, 0o600)
    except OSError:
        pass


def _find_alias(name: str) -> dict | None:
    for a in _load_aliases().get("aliases", []):
        if a.get("alias") == name:
            return a
    return None


# ── Connection pool (paramiko) ─────────────────────────────────────────────────

_pool: dict[str, dict] = {}            # alias -> {client, sftp, last_used}
_sudo_pw_cache: dict[str, tuple] = {}  # alias -> (password, ts)
_login_pw_cache: dict[str, tuple] = {} # "alias:login" | "alias:passphrase" -> (secret, ts)


def _login_password(alias: str, kind: str = "login") -> str | None:
    """Return the SSH login password (``kind="login"``) or private-key passphrase
    (``kind="passphrase"``) for ``alias`` from the RAM cache, or elicit it.

    Never persisted. Returns None if the user declines/cancels/times out.
    """
    now = time.time()
    key = f"{alias}:{kind}"
    cached = _login_pw_cache.get(key)
    if cached and (now - cached[1] <= LOGIN_PW_TTL):
        return cached[0]

    if kind == "passphrase":
        message = f"Enter the passphrase for the private key of SSH alias '{alias}'."
        title = f"key passphrase — {alias}"
    else:
        message = f"Enter the SSH login password for alias '{alias}'."
        title = f"SSH password — {alias}"

    result = elicit(
        message,
        {
            "type": "object",
            "properties": {
                "password": {"type": "string", "format": "password", "title": title}
            },
            "required": ["password"],
        },
    )
    if result.get("action") == "accept":
        pw = (result.get("content") or {}).get("password", "")
        _login_pw_cache[key] = (pw, now)
        return pw
    return None


def _clear_login_pw(alias: str) -> None:
    """Drop any cached login password / passphrase for ``alias``."""
    for k in [k for k in _login_pw_cache if k.startswith(f"{alias}:")]:
        _login_pw_cache.pop(k, None)


def _is_auth_failure(paramiko, e: Exception) -> bool:
    """True if ``e`` is an SSH auth rejection a login password could resolve.

    ``AuthenticationException`` (wrong/refused key) always qualifies. A plain
    ``SSHException`` qualifies only when its message says paramiko had no method
    to try — e.g. a password-only host with no key/agent: *"No authentication
    methods available"*. Other SSH errors (banner, host key, protocol) do not.
    """
    if isinstance(e, paramiko.AuthenticationException):
        return True
    msg = str(e).lower()
    return "authentication method" in msg or "no authentication" in msg


def _require_paramiko():
    try:
        import paramiko  # type: ignore
        return paramiko
    except ImportError:
        raise ToolError(
            "paramiko not installed — add 'paramiko>=3.4' to requirements.txt "
            "and reinstall the .venv (uv pip install -r requirements.txt)."
        )


def _connect(cfg: dict, paramiko):
    alias = cfg.get("alias", "")
    auth = (cfg.get("auth") or "key").lower()

    identity = cfg.get("identity_file")
    identity = os.path.expanduser(identity) if identity else None

    password = None
    if auth == "password":
        password = _login_password(alias, "login")
        if password is None:
            raise ToolError(
                f"login password required for alias '{alias}' (user declined or timed out)"
            )

    def attempt(passphrase):
        # With a password in hand, skip agent/key probing so paramiko goes
        # straight to password auth instead of failing on keys first.
        use_pw = password is not None
        client = paramiko.SSHClient()
        client.load_system_host_keys()
        known = os.path.expanduser("~/.ssh/known_hosts")
        if os.path.exists(known):
            try:
                client.load_host_keys(known)
            except Exception as e:
                log(f"could not load known_hosts: {e}")
        if cfg.get("accept_new_host_key"):
            client.set_missing_host_key_policy(paramiko.AutoAddPolicy())
        else:
            client.set_missing_host_key_policy(paramiko.RejectPolicy())
        client.connect(
            hostname=cfg["hostname"],
            port=int(cfg.get("port", 22)),
            username=cfg.get("username"),
            password=password,
            key_filename=identity,
            passphrase=passphrase,
            allow_agent=not use_pw,
            look_for_keys=not use_pw,
            timeout=CONNECT_TIMEOUT,
        )
        return client

    passphrase = os.environ.get("SSH_MCP_KEY_PASSPHRASE") or None
    try:
        return attempt(passphrase)
    except paramiko.PasswordRequiredException:
        # Encrypted private key with no passphrase supplied — ask for it (lazy).
        if passphrase is not None:
            raise   # we already had one and it was rejected; don't loop
        passphrase = _login_password(alias, "passphrase")
        if passphrase is None:
            raise ToolError(
                f"key passphrase required for alias '{alias}' (user declined or timed out)"
            )
        return attempt(passphrase)
    except (paramiko.AuthenticationException, paramiko.SSHException) as e:
        # Key/agent auth was rejected, or the host offers no method paramiko
        # could try (e.g. a password-only host: "No authentication methods
        # available"). If we haven't tried a password yet, elicit one and retry.
        # Declining re-raises the original error. Covers aliases left as the
        # default auth=key that actually need a login password.
        if password is not None or not _is_auth_failure(paramiko, e):
            raise
        password = _login_password(alias, "login")
        if password is None:
            raise
        return attempt(passphrase)


def _close(alias: str) -> None:
    entry = _pool.pop(alias, None)
    if not entry:
        return
    try:
        if entry.get("sftp"):
            entry["sftp"].close()
    except Exception:
        pass
    try:
        entry["client"].close()
    except Exception:
        pass


def _get_client(alias: str):
    cfg = _find_alias(alias)
    if not cfg:
        raise ToolError(f"unknown alias '{alias}'")
    paramiko = _require_paramiko()
    now = time.time()

    entry = _pool.get(alias)
    if entry:
        t = entry["client"].get_transport()
        if (now - entry["last_used"] <= POOL_TTL) and t is not None and t.is_active():
            entry["last_used"] = now
            return entry["client"]
        _close(alias)

    try:
        client = _connect(cfg, paramiko)
    except paramiko.AuthenticationException:
        _clear_login_pw(alias)   # wrong password/passphrase → re-prompt next time
        raise ToolError(f"authentication failed for alias '{alias}' (check key/agent/password)")
    except paramiko.BadHostKeyException:
        raise ToolError(
            f"host key mismatch for alias '{alias}' (possible MITM) — fix ~/.ssh/known_hosts"
        )
    except paramiko.SSHException as e:
        if "not found in known_hosts" in str(e):
            raise ToolError(
                f"unknown host key for alias '{alias}' — re-add it with "
                f"accept_new_host_key=true to trust it on first connect"
            )
        raise ToolError(f"SSH error for alias '{alias}': {e}")
    except (OSError, socket.error) as e:
        raise ToolError(f"connection to alias '{alias}' failed: {e}")

    _pool[alias] = {"client": client, "sftp": None, "last_used": now}
    return client


def _get_sftp(alias: str):
    client = _get_client(alias)
    entry = _pool[alias]
    if entry.get("sftp") is None:
        entry["sftp"] = client.open_sftp()
    return entry["sftp"]


def _run_with_stdin(client, command: str, timeout: int, stdin_data: str | None = None):
    """Run a remote command; return (stdout, stderr, exit_code). Raises on timeout."""
    try:
        chan_in, chan_out, chan_err = client.exec_command(command, timeout=timeout)
        if stdin_data is not None:
            try:
                chan_in.write(stdin_data)
                chan_in.flush()
            except Exception:
                pass
        out = chan_out.read().decode("utf-8", "replace")
        err = chan_err.read().decode("utf-8", "replace")
        code = chan_out.channel.recv_exit_status()
        return out, err, code
    except socket.timeout:
        raise ToolError(f"command timed out after {timeout}s")


# ── sudo ───────────────────────────────────────────────────────────────────────

def _sudo_password(alias: str) -> str | None:
    """Return the sudo password for ``alias`` from RAM cache, or elicit it.

    Never persisted. Returns None if the user declines/cancels/times out.
    """
    now = time.time()
    cached = _sudo_pw_cache.get(alias)
    if cached and (now - cached[1] <= SUDO_PW_TTL):
        return cached[0]

    result = elicit(
        f"Enter the sudo password for SSH alias '{alias}'.",
        {
            "type": "object",
            "properties": {
                "password": {
                    "type": "string",
                    "format": "password",
                    "title": f"sudo password — {alias}",
                }
            },
            "required": ["password"],
        },
    )
    if result.get("action") == "accept":
        pw = (result.get("content") or {}).get("password", "")
        _sudo_pw_cache[alias] = (pw, now)
        return pw
    return None


def _sudo_prefix(alias: str, cfg: dict, sudo_user: str | None):
    """Build the sudo prefix for ``cfg``. Returns (prefix, stdin_password).

    Raises ToolError when sudo is disabled or the password is unavailable.
    """
    method = (cfg.get("sudo") or {}).get("method", "prompt")
    u = f"-u {shlex.quote(sudo_user)} " if sudo_user else ""
    if method == "none":
        raise ToolError(f"sudo is disabled for alias '{alias}'")
    if method == "nopasswd":
        return f"sudo -n {u}", None
    pw = _sudo_password(alias)
    if pw is None:
        raise ToolError("sudo password required (user declined or timed out)")
    return f"sudo -S -p '' {u}", pw


# ── SFTP helpers ───────────────────────────────────────────────────────────────

def _sftp_read_text(sftp, path: str) -> str:
    with sftp.open(path, "r") as f:
        data = f.read()
    return data.decode("utf-8", "replace") if isinstance(data, (bytes, bytearray)) else data


def _sftp_write_atomic(sftp, path: str, content: str) -> None:
    """Write atomically: temp file in the same dir + posix_rename. Preserve mode."""
    d = posixpath.dirname(path) or "."
    base = posixpath.basename(path)
    tmp = posixpath.join(d, f".{base}.tmp.{os.getpid()}")

    mode = None
    try:
        mode = stat.S_IMODE(sftp.stat(path).st_mode)
    except IOError:
        pass

    with sftp.open(tmp, "w") as f:
        f.write(content)
    if mode is not None:
        try:
            sftp.chmod(tmp, mode)
        except IOError:
            pass
    try:
        sftp.posix_rename(tmp, path)
    except (IOError, AttributeError):
        try:
            sftp.remove(path)
        except IOError:
            pass
        sftp.rename(tmp, path)


def _sftp_mkdirs(sftp, d: str) -> None:
    if not d or d in ("/", "."):
        return
    try:
        sftp.stat(d)
        return
    except IOError:
        pass
    parent = posixpath.dirname(d)
    if parent and parent != d:
        _sftp_mkdirs(sftp, parent)
    try:
        sftp.mkdir(d)
    except IOError:
        pass


def _relpath(root: str, full: str) -> str:
    r = root.rstrip("/") or "/"
    return posixpath.relpath(full, r)


# ── Tools: aliases ─────────────────────────────────────────────────────────────

def _tool_list_aliases(args: dict) -> str:
    out = []
    for a in _load_aliases().get("aliases", []):
        out.append({
            "alias": a.get("alias"),
            "hostname": a.get("hostname"),
            "port": a.get("port", 22),
            "username": a.get("username"),
            "auth": a.get("auth", "key"),
            "sudo_method": (a.get("sudo") or {}).get("method", "prompt"),
            "description": a.get("description", ""),
        })
    return json.dumps(out, indent=2)


def _tool_add_alias(args: dict) -> str:
    name = args.get("alias")
    if not name:
        return "Error: missing required argument: alias"
    if not args.get("hostname"):
        return "Error: missing required argument: hostname"

    sudo = args.get("sudo")
    method = sudo.get("method") if isinstance(sudo, dict) else (sudo or "prompt")
    if method not in ("nopasswd", "prompt", "none"):
        return f"Error: invalid sudo method '{method}' (use nopasswd|prompt|none)"

    auth = (args.get("auth") or "key").lower()
    if auth not in ("key", "password"):
        return f"Error: invalid auth method '{auth}' (use key|password)"

    entry = {
        "alias": name,
        "hostname": args["hostname"],
        "port": int(args.get("port", 22)),
        "username": args.get("username"),
        "identity_file": args.get("identity_file"),
        "description": args.get("description", ""),
        "auth": auth,
        "sudo": {"method": method},
        "accept_new_host_key": bool(args.get("accept_new_host_key", False)),
    }

    data = _load_aliases()
    aliases = data.setdefault("aliases", [])
    prev = None
    for i, a in enumerate(aliases):
        if a.get("alias") == name:
            prev = a
            aliases[i] = entry
            break
    else:
        aliases.append(entry)
    _save_aliases(data)
    _close(name)          # config may have changed — drop any pooled connection
    _sudo_pw_cache.pop(name, None)
    _clear_login_pw(name)

    target = f"{entry.get('username')}@{entry['hostname']}:{entry['port']}"
    if prev:
        return f"Updated alias '{name}' → {target} (auth: {auth}, sudo: {method})."
    return f"Added alias '{name}' → {target} (auth: {auth}, sudo: {method})."


def _tool_remove_alias(args: dict) -> str:
    name = args.get("alias")
    if not name:
        return "Error: missing required argument: alias"
    data = _load_aliases()
    aliases = data.get("aliases", [])
    kept = [a for a in aliases if a.get("alias") != name]
    if len(kept) == len(aliases):
        return f"Error: alias '{name}' not found"
    data["aliases"] = kept
    _save_aliases(data)
    _close(name)
    _sudo_pw_cache.pop(name, None)
    _clear_login_pw(name)
    return f"Removed alias '{name}'."


# ── Tools: filesystem (native output format) ───────────────────────────────────

def _tool_read_file(args: dict) -> str:
    alias, path = args.get("alias"), args.get("path")
    if not alias or not path:
        return "Error: 'alias' and 'path' are required"
    sftp = _get_sftp(alias)
    try:
        content = _sftp_read_text(sftp, path)
    except IOError as e:
        raise ToolError(f"cannot read {path}: {e}")

    lines = content.splitlines()
    total = len(lines)

    limit = args.get("limit")
    limit = min(int(limit), MAX_READ_LINES) if limit is not None else None
    start = max(int(args["start_line"]) - 1, 0) if args.get("start_line") is not None else 0
    if args.get("end_line") is not None:
        end = min(int(args["end_line"]), total)
    elif limit is not None:
        end = min(start + limit, total)
    else:
        end = total

    if start >= total and total > 0:
        return f"(file has only {total} lines; start_line {start + 1} is out of range)"
    end = max(end, start)

    width = max(len(str(total)), 3)
    return "\n".join(
        f"{start + i + 1:>{width}} | {line}" for i, line in enumerate(lines[start:end])
    )


def _tool_list_files(args: dict) -> str:
    alias, path = args.get("alias"), args.get("path")
    if not alias or not path:
        return "Error: 'alias' and 'path' are required"
    max_depth = int(args.get("depth", 3))
    dirs_only = bool(args.get("dirs_only", False))
    sftp = _get_sftp(alias)

    out: list[str] = []

    def walk(d: str, depth: int) -> None:
        try:
            entries = sftp.listdir_attr(d)
        except IOError:
            return
        for a in entries:
            full = posixpath.join(d, a.filename)
            if stat.S_ISDIR(a.st_mode):
                if a.filename in SKIP_DIRS:
                    continue
                if dirs_only:
                    out.append(_relpath(path, full))
                if depth + 1 < max_depth:
                    walk(full, depth + 1)
            elif stat.S_ISREG(a.st_mode) and not dirs_only:
                out.append(_relpath(path, full))

    try:
        sftp.listdir_attr(path)
    except IOError as e:
        raise ToolError(f"cannot list {path}: {e}")
    walk(path, 0)
    out.sort()
    return json.dumps(out)


def _grep_flags(args: dict) -> str:
    flags = ""
    if not bool(args.get("case_sensitive", False)):
        flags += "-i "
    inc = args.get("include_glob")
    if inc:
        flags += f"--include={shlex.quote(inc)} "
    return flags


def _tool_grep_files(args: dict) -> str:
    alias, path, pattern = args.get("alias"), args.get("path"), args.get("pattern")
    if not alias or not path or pattern is None:
        return "Error: 'alias', 'path' and 'pattern' are required"
    mode = args.get("output_mode", "content")
    ctx = min(int(args.get("context_lines", 0) or 0), 10)
    maxr = int(args.get("max_results", 100))
    client = _get_client(alias)

    flags = _grep_flags(args)
    qpat, qpath = shlex.quote(pattern), shlex.quote(path)
    root_prefix = path.rstrip("/") + "/"

    def rel(p: str) -> str:
        return p[len(root_prefix):] if p.startswith(root_prefix) else p

    if mode == "files_only":
        cmd = f"grep -rlIZ {flags}-E -e {qpat} -- {qpath}"
        out, err, code = _run_with_stdin(client, cmd, DEFAULT_CMD_TIMEOUT)
        if code >= 2 and not out:
            raise ToolError(err.strip() or "grep failed")
        files = [rel(f) for f in out.split("\0") if f][:maxr]
        if not files:
            return f'No files match "{pattern}" in {path}.'
        return f"{len(files)} file(s):\n" + "\n".join(files)

    if mode == "count":
        cmd = f"grep -rcI {flags}-E -e {qpat} -- {qpath}"
        out, err, code = _run_with_stdin(client, cmd, DEFAULT_CMD_TIMEOUT)
        if code >= 2 and not out:
            raise ToolError(err.strip() or "grep failed")
        items = []
        for line in out.splitlines():
            f, _, c = line.rpartition(":")     # rpartition: count is numeric at end
            if f and c.isdigit() and int(c) > 0:
                items.append((rel(f), int(c)))
        items = items[:maxr]
        if not items:
            return f'No matches for "{pattern}" in {path}.'
        return f"{len(items)} file(s):\n" + "\n".join(f"{f}: {c}" for f, c in items)

    # content mode
    cflag = f"-C {ctx} " if ctx else ""
    cmd = f"grep -rnIZ {cflag}{flags}-E -e {qpat} -- {qpath}"
    out, err, code = _run_with_stdin(client, cmd, DEFAULT_CMD_TIMEOUT)
    if code >= 2 and not out:
        raise ToolError(err.strip() or "grep failed")

    entries: list[str] = []
    if ctx == 0:
        for line in out.split("\n"):
            if not line:
                continue
            if "\0" in line:
                f, _, rest = line.partition("\0")
            else:
                f, _, rest = line.partition(":")
            lineno, _, body = rest.partition(":")
            entries.append(f"{rel(f)}:{lineno}: {body}")
            if len(entries) >= maxr:
                break
    else:
        prev_file = None
        for line in out.split("\n"):
            if not line:
                continue
            if line == "--":
                if prev_file is not None and len(entries) < maxr:
                    entries.append(f"{rel(prev_file)}:---")
                continue
            if "\0" in line:
                f, _, rest = line.partition("\0")
            else:
                f, _, rest = line.partition(":")
            m = re.match(r"(\d+)([:-])(.*)$", rest, re.S)
            if not m:
                continue
            lineno, sep, body = m.group(1), m.group(2), m.group(3)
            marker = ">" if sep == ":" else " "
            entries.append(f"{marker}{rel(f)}: {lineno}: {body}")
            prev_file = f
            if len(entries) >= maxr:
                break

    if not entries:
        return f'No matches for "{pattern}" in {path}.'
    return f"{len(entries)} match(es):\n" + "\n".join(entries)


def _tool_edit_file(args: dict) -> str:
    alias, path = args.get("alias"), args.get("path")
    old, new = args.get("old"), args.get("new")
    if not alias or not path:
        return "Error: 'alias' and 'path' are required"
    if old is None or new is None:
        return "Error: 'old' and 'new' are required"
    replace_all = bool(args.get("replace_all", False))
    sftp = _get_sftp(alias)
    try:
        content = _sftp_read_text(sftp, path)
    except IOError as e:
        raise ToolError(f"cannot read {path}: {e}")

    not_found = (
        f"Error: Text not found in {path}. "
        f"Call read_file first and copy the text exactly as shown after the '| ' prefix."
    )
    if replace_all:
        if old not in content:
            return not_found
        updated = content.replace(old, new)
    else:
        cnt = content.count(old)
        if cnt > 1:
            return (
                f"Error: Text found {cnt} times in {path}. "
                f"Include more surrounding context in `old` to make it unique, "
                f"or set replace_all=true."
            )
        if cnt == 0:
            return not_found
        updated = content.replace(old, new, 1)

    _sftp_write_atomic(sftp, path, updated)
    return f"Edited {path}."


def _tool_replace_lines(args: dict) -> str:
    alias, path = args.get("alias"), args.get("path")
    if not alias or not path:
        return "Error: 'alias' and 'path' are required"
    if args.get("from_line") is None or args.get("to_line") is None or args.get("new") is None:
        return "Error: 'from_line', 'to_line' and 'new' are required"
    from_line = int(args["from_line"])
    to_line = int(args["to_line"])
    new = args["new"]
    if from_line < 1:
        return "Error: from_line must be >= 1"
    if to_line < from_line:
        return "Error: to_line must be >= from_line"

    sftp = _get_sftp(alias)
    try:
        content = _sftp_read_text(sftp, path)
    except IOError as e:
        raise ToolError(f"cannot read {path}: {e}")

    lines = content.splitlines()
    total = len(lines)
    if from_line > total:
        return f"Error: from_line {from_line} exceeds file length ({total} lines)"
    to_clamped = min(to_line, total)
    new_lines = new.splitlines()
    lines[from_line - 1:to_clamped] = new_lines

    updated = "\n".join(lines)
    if content.endswith("\n"):
        updated += "\n"
    _sftp_write_atomic(sftp, path, updated)
    return f"Replaced lines {from_line}–{to_clamped} in {path} with {len(new_lines)} new lines."


# ── Tools: exec / sudo / systemd ────────────────────────────────────────────────

def _tool_exec(args: dict) -> str:
    alias, command = args.get("alias"), args.get("command")
    if not alias or command is None:
        return "Error: 'alias' and 'command' are required"
    sudo = bool(args.get("sudo", False))
    sudo_user = args.get("sudo_user")
    timeout = int(args.get("timeout_sec", DEFAULT_CMD_TIMEOUT))
    cfg = _find_alias(alias)
    if not cfg:
        return f"Error: unknown alias '{alias}'"

    pw = None
    wrapped = command
    if sudo:
        prefix, pw = _sudo_prefix(alias, cfg, sudo_user)
        wrapped = prefix + command

    client = _get_client(alias)
    try:
        chan_in, chan_out, chan_err = client.exec_command(wrapped, timeout=timeout)
        if pw is not None:
            try:
                chan_in.write(pw + "\n")
                chan_in.flush()
            except Exception:
                pass
        out = chan_out.read().decode("utf-8", "replace")
        err = chan_err.read().decode("utf-8", "replace")
        code = chan_out.channel.recv_exit_status()
    except socket.timeout:
        return f"Error: command timed out after {timeout}s"
    return json.dumps({"stdout": out, "stderr": err, "exit_code": code})


def _tool_systemd(args: dict) -> str:
    alias, service, action = args.get("alias"), args.get("service"), args.get("action")
    if not alias or not service or not action:
        return "Error: 'alias', 'service' and 'action' are required"
    allowed = {"status", "start", "stop", "restart", "reload", "enable", "disable"}
    if action not in allowed:
        return f"Error: invalid action '{action}' (allowed: {', '.join(sorted(allowed))})"
    cfg = _find_alias(alias)
    if not cfg:
        return f"Error: unknown alias '{alias}'"

    qsvc = shlex.quote(service)
    client = _get_client(alias)

    parts: list[str] = []
    if action != "status":
        prefix, pw = _sudo_prefix(alias, cfg, None)
        out, err, code = _run_with_stdin(
            client, f"{prefix}systemctl {action} {qsvc}", DEFAULT_CMD_TIMEOUT,
            (pw + "\n") if pw else None,
        )
        parts.append(f"$ systemctl {action} {service}  (exit {code})")
        if out.strip():
            parts.append(out.strip())
        if err.strip():
            parts.append(err.strip())

    status, _, _ = _run_with_stdin(
        client, f"systemctl status {qsvc} --no-pager 2>&1 | head -n 20", DEFAULT_CMD_TIMEOUT)
    parts.append("── status ──")
    parts.append(status.strip())

    journal, _, _ = _run_with_stdin(
        client, f"journalctl -u {qsvc} -n 10 --no-pager 2>&1", DEFAULT_CMD_TIMEOUT)
    parts.append("── journal (last 10) ──")
    parts.append(journal.strip())
    return "\n".join(parts)


# ── Tools: transfer / diagnostics ───────────────────────────────────────────────

def _tool_upload(args: dict) -> str:
    alias = args.get("alias")
    local_path, remote_path = args.get("local_path"), args.get("remote_path")
    if not alias or not local_path or not remote_path:
        return "Error: 'alias', 'local_path' and 'remote_path' are required"
    if not os.path.exists(local_path):
        return f"Error: local path not found: {local_path}"
    sftp = _get_sftp(alias)

    count = total = 0
    dest_shown = remote_path
    if os.path.isdir(local_path):
        for root, _dirs, files in os.walk(local_path):
            relroot = os.path.relpath(root, local_path)
            rdir = remote_path if relroot == "." else posixpath.join(
                remote_path, relroot.replace(os.sep, "/"))
            _sftp_mkdirs(sftp, rdir)
            for fn in files:
                lf = os.path.join(root, fn)
                sftp.put(lf, posixpath.join(rdir, fn))
                count += 1
                total += os.path.getsize(lf)
    else:
        # scp/rsync semantics: a trailing-slash or existing-directory remote_path
        # means "upload the file INTO that directory". paramiko's sftp.put needs a
        # full destination FILE path — handed a directory path it fails with a
        # generic "Failure" — so append the local basename in that case.
        into_dir = remote_path.endswith("/")
        if not into_dir:
            try:
                into_dir = stat.S_ISDIR(sftp.stat(remote_path).st_mode)
            except IOError:
                into_dir = False
        if into_dir:
            target_dir = remote_path.rstrip("/") or "/"
            _sftp_mkdirs(sftp, target_dir)
            dest = posixpath.join(target_dir, os.path.basename(local_path))
        else:
            dest = remote_path
            parent = posixpath.dirname(dest)
            if parent:
                _sftp_mkdirs(sftp, parent)
        sftp.put(local_path, dest)
        count, total = 1, os.path.getsize(local_path)
        dest_shown = dest
    return f"Uploaded {count} file(s), {total} bytes → {dest_shown}"


def _tool_download(args: dict) -> str:
    alias = args.get("alias")
    remote_path, local_path = args.get("remote_path"), args.get("local_path")
    if not alias or not remote_path or not local_path:
        return "Error: 'alias', 'remote_path' and 'local_path' are required"
    sftp = _get_sftp(alias)
    try:
        st = sftp.stat(remote_path)
    except IOError as e:
        raise ToolError(f"remote path not found: {remote_path} ({e})")

    count = total = 0
    if stat.S_ISDIR(st.st_mode):
        def rec(rdir: str, ldir: str) -> None:
            nonlocal count, total
            os.makedirs(ldir, exist_ok=True)
            for a in sftp.listdir_attr(rdir):
                rf = posixpath.join(rdir, a.filename)
                lf = os.path.join(ldir, a.filename)
                if stat.S_ISDIR(a.st_mode):
                    rec(rf, lf)
                elif stat.S_ISREG(a.st_mode):
                    sftp.get(rf, lf)
                    count += 1
                    total += a.st_size or os.path.getsize(lf)
        rec(remote_path, local_path)
    else:
        parent = os.path.dirname(local_path)
        if parent:
            os.makedirs(parent, exist_ok=True)
        sftp.get(remote_path, local_path)
        count, total = 1, os.path.getsize(local_path)
    return f"Downloaded {count} file(s), {total} bytes → {local_path}"


def _tool_sysinfo(args: dict) -> str:
    alias = args.get("alias")
    if not alias:
        return "Error: 'alias' is required"
    client = _get_client(alias)
    cmd = (
        "echo OS=$(uname -s 2>/dev/null); "
        "echo KERNEL=$(uname -r 2>/dev/null); "
        "echo CPU=$(nproc 2>/dev/null); "
        "echo MEMTOTAL=$(awk '/MemTotal/{print $2}' /proc/meminfo 2>/dev/null); "
        "echo MEMAVAIL=$(awk '/MemAvailable/{print $2}' /proc/meminfo 2>/dev/null); "
        "echo DISKTOTAL=$(df -kP / 2>/dev/null | tail -1 | awk '{print $2}'); "
        "echo DISKAVAIL=$(df -kP / 2>/dev/null | tail -1 | awk '{print $4}'); "
        "echo UPTIME=$(uptime -p 2>/dev/null || uptime 2>/dev/null)"
    )
    out, _, _ = _run_with_stdin(client, cmd, DEFAULT_CMD_TIMEOUT)
    kv: dict[str, str] = {}
    for line in out.splitlines():
        if "=" in line:
            k, _, v = line.partition("=")
            kv[k.strip()] = v.strip()

    def gb(key: str):
        try:
            return round(int(kv.get(key, "")) / 1024 / 1024, 2)
        except (ValueError, TypeError):
            return None

    info = {
        "os": kv.get("OS", ""),
        "kernel": kv.get("KERNEL", ""),
        "cpu_count": int(kv["CPU"]) if kv.get("CPU", "").isdigit() else None,
        "ram_total_gb": gb("MEMTOTAL"),
        "ram_free_gb": gb("MEMAVAIL"),
        "disk_total_gb": gb("DISKTOTAL"),
        "disk_free_gb": gb("DISKAVAIL"),
        "uptime": kv.get("UPTIME", ""),
    }
    return json.dumps(info, indent=2)


# ── Tool registry ────────────────────────────────────────────────────────────────

_ALIAS = {"type": "string", "description": "Host alias registered via add_alias."}
_SFTP_NOTE = (
    " Runs as the login user (no sudo): for paths needing root, use exec with "
    "sudo=true (e.g. tee/install)."
)

TOOLS = [
    {
        "name": "list_aliases",
        "description": "List configured SSH host aliases (never reveals keys or sudo passwords).",
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "add_alias",
        "description": "Register or update an SSH host alias. Login via SSH key/agent (default) or login password asked on demand via elicitation.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alias": {"type": "string", "description": "Short name used to address the host."},
                "hostname": {"type": "string", "description": "Host or IP."},
                "port": {"type": "integer", "description": "SSH port (default 22)."},
                "username": {"type": "string", "description": "Login user."},
                "identity_file": {"type": "string", "description": "Path to private key (optional; ssh-agent is also tried). An encrypted key's passphrase is asked via elicitation."},
                "description": {"type": "string", "description": "Free-text note."},
                "auth": {"type": "string", "enum": ["key", "password"],
                         "description": "Login auth. key: SSH key/agent (default). password: login password asked on demand via elicitation, kept only in RAM."},
                "sudo": {"type": "string", "enum": ["nopasswd", "prompt", "none"],
                         "description": "How sudo authenticates. Use 'prompt' unless you KNOW otherwise — it is the safe default: runs 'sudo -S' and asks the user for the sudo password on demand via elicitation, so it works on any host where the login user is a normal sudoer. Only pick 'nopasswd' when the remote /etc/sudoers actually grants THIS user passwordless sudo (a NOPASSWD: rule): it runs 'sudo -n' and NEVER prompts, so on a normal host every sudo call fails immediately with 'a password is required'. 'none' disables sudo. Default prompt."},
                "accept_new_host_key": {"type": "boolean", "description": "Trust the host key on first connect (TOFU). Default false."},
            },
            "required": ["alias", "hostname", "username"],
        },
    },
    {
        "name": "remove_alias",
        "description": "Remove a host alias and close its pooled connection.",
        "inputSchema": {"type": "object", "properties": {"alias": _ALIAS}, "required": ["alias"]},
    },
    {
        "name": "read_file",
        "description": "Read a remote file with 1-based line numbers (same format as the local read_file)." + _SFTP_NOTE,
        "inputSchema": {
            "type": "object",
            "properties": {
                "alias": _ALIAS,
                "path": {"type": "string", "description": "Absolute remote path."},
                "start_line": {"type": "integer", "description": "First line (1-based, inclusive)."},
                "end_line": {"type": "integer", "description": "Last line (1-based, inclusive)."},
                "limit": {"type": "integer", "description": "Max lines to read (cap 2000)."},
            },
            "required": ["alias", "path"],
        },
    },
    {
        "name": "list_files",
        "description": "List files/dirs under a remote path; returns a JSON array of relative paths (same as local list_files).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alias": _ALIAS,
                "path": {"type": "string", "description": "Absolute remote directory."},
                "depth": {"type": "integer", "description": "Max recursion depth (default 3; 1 = immediate contents)."},
                "dirs_only": {"type": "boolean", "description": "Only directories (default false)."},
            },
            "required": ["alias", "path"],
        },
    },
    {
        "name": "grep_files",
        "description": "Search a remote path with a regex; output matches the local grep_files (uses remote grep -E).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alias": _ALIAS,
                "path": {"type": "string", "description": "Remote file or directory."},
                "pattern": {"type": "string", "description": "Regex (case-insensitive by default)."},
                "case_sensitive": {"type": "boolean", "description": "Default false."},
                "include_glob": {"type": "string", "description": "Restrict to files matching this glob, e.g. '*.rs'."},
                "output_mode": {"type": "string", "enum": ["content", "files_only", "count"], "description": "Default 'content'."},
                "context_lines": {"type": "integer", "description": "Lines of context per match (default 0, max 10)."},
                "max_results": {"type": "integer", "description": "Stop after N results (default 100)."},
            },
            "required": ["alias", "path", "pattern"],
        },
    },
    {
        "name": "edit_file",
        "description": "Find & replace in a remote file (atomic). `old` must match exactly once unless replace_all." + _SFTP_NOTE,
        "inputSchema": {
            "type": "object",
            "properties": {
                "alias": _ALIAS,
                "path": {"type": "string", "description": "Absolute remote path."},
                "old": {"type": "string", "description": "Exact text to replace."},
                "new": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."},
            },
            "required": ["alias", "path", "old", "new"],
        },
    },
    {
        "name": "replace_lines",
        "description": "Replace a 1-based inclusive line range in a remote file (atomic)." + _SFTP_NOTE,
        "inputSchema": {
            "type": "object",
            "properties": {
                "alias": _ALIAS,
                "path": {"type": "string", "description": "Absolute remote path."},
                "from_line": {"type": "integer", "description": "First line (1-based, inclusive)."},
                "to_line": {"type": "integer", "description": "Last line (1-based, inclusive)."},
                "new": {"type": "string", "description": "Replacement text."},
            },
            "required": ["alias", "path", "from_line", "to_line", "new"],
        },
    },
    {
        "name": "exec",
        "description": "Run a command on the remote host. Set sudo=true to run via sudo (method per alias).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alias": _ALIAS,
                "command": {"type": "string", "description": "Shell command."},
                "sudo": {"type": "boolean", "description": "Run via sudo (default false)."},
                "sudo_user": {"type": "string", "description": "Target user for sudo -u (optional)."},
                "timeout_sec": {"type": "integer", "description": "Kill after N seconds (default 120)."},
            },
            "required": ["alias", "command"],
        },
    },
    {
        "name": "upload",
        "description": "Upload a local file or directory (recursive) to the remote host via SFTP." + _SFTP_NOTE,
        "inputSchema": {
            "type": "object",
            "properties": {
                "alias": _ALIAS,
                "local_path": {"type": "string", "description": "Local file or directory."},
                "remote_path": {"type": "string", "description": "Remote destination. For a single file: a trailing '/' (or an existing remote directory) uploads the file INTO that directory keeping its name; otherwise it is the exact destination file path (parent dirs are created)."},
            },
            "required": ["alias", "local_path", "remote_path"],
        },
    },
    {
        "name": "download",
        "description": "Download a remote file or directory (recursive) to the local host via SFTP.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alias": _ALIAS,
                "remote_path": {"type": "string", "description": "Remote file or directory."},
                "local_path": {"type": "string", "description": "Local destination path."},
            },
            "required": ["alias", "remote_path", "local_path"],
        },
    },
    {
        "name": "sysinfo",
        "description": "Report OS, kernel, CPU count, RAM and root-disk usage, and uptime.",
        "inputSchema": {"type": "object", "properties": {"alias": _ALIAS}, "required": ["alias"]},
    },
    {
        "name": "systemd",
        "description": "Manage a systemd service (status/start/stop/restart/reload/enable/disable) + last 10 journal lines. Mutating actions use sudo.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alias": _ALIAS,
                "service": {"type": "string", "description": "Service/unit name."},
                "action": {"type": "string", "enum": ["status", "start", "stop", "restart", "reload", "enable", "disable"]},
            },
            "required": ["alias", "service", "action"],
        },
    },
]

TOOL_DISPATCH = {
    "list_aliases": _tool_list_aliases,
    "add_alias": _tool_add_alias,
    "remove_alias": _tool_remove_alias,
    "read_file": _tool_read_file,
    "list_files": _tool_list_files,
    "grep_files": _tool_grep_files,
    "edit_file": _tool_edit_file,
    "replace_lines": _tool_replace_lines,
    "exec": _tool_exec,
    "upload": _tool_upload,
    "download": _tool_download,
    "sysinfo": _tool_sysinfo,
    "systemd": _tool_systemd,
}


# ── JSON-RPC dispatch ────────────────────────────────────────────────────────────

def handle_message(msg: dict) -> dict | None:
    method = msg.get("method", "")
    req_id = msg.get("id")

    if method == "initialize":
        return _ok(req_id, {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "ssh", "version": "1.0.0"},
        })
    if method == "notifications/initialized":
        return None
    if method == "tools/list":
        return _ok(req_id, {"tools": TOOLS})
    if method == "tools/call":
        params = msg.get("params", {})
        name = params.get("name", "")
        targs = params.get("arguments", {}) or {}
        handler = TOOL_DISPATCH.get(name)
        if handler is None:
            return _text_result(req_id, f"Error: Unknown tool: {name}", True)
        try:
            text = handler(targs)
        except ToolError as e:
            text = f"Error: {e}"
        except Exception as e:
            log(f"unhandled exception in tool '{name}': {e}")
            text = f"Error: internal error in '{name}': {e}"
        return _text_result(req_id, text, text.startswith("Error:"))

    if req_id is not None:
        return {"jsonrpc": "2.0", "id": req_id,
                "error": {"code": -32601, "message": f"Method not found: {method}"}}
    return None


def main() -> None:
    log("starting SSH MCP server")
    try:
        while True:
            msg = readline()
            if msg is None:
                break
            resp = handle_message(msg)
            if resp is not None:
                send(resp)
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
