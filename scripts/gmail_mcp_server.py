#!/usr/bin/env python3
"""Google Gmail MCP server (JSON-RPC 2.0 over stdio).

Capabilities (callable as `mcp__gmail__<tool>`):
  status             — self-check: credentials, token refresh, API reachability
  list_messages      — list messages with optional query / label filter
  get_message        — read a single message by ID (with optional body)
  get_thread         — read all messages in a thread
  list_labels        — list labels/folders with message counts
  modify_message     — add/remove labels (mark read, archive, star)
  send_message       — send an email (supports in-thread replies + file attachments)
  get_profile        — account info (email, totals)
  create_label       — create a new label
  download_attachments — save all attachments from a message to disk

Provides read, modify, and send access to Gmail via the Gmail API v1.

Credentials are read from ./secrets/gmail_creds.json by default.
Override with GMAIL_CREDS_PATH env var.

Run scripts/gmail_oauth_setup.py first to generate the OAuth token.
"""

from __future__ import annotations

import base64
import json
import mimetypes
import os
import re
import sys
import threading
import time
from email.message import EmailMessage
from html.parser import HTMLParser
from typing import Any, Callable

# Log to stderr so stdout stays clean for JSON-RPC.
def log(msg: str) -> None:
    print(f"[gmail_mcp] {msg}", file=sys.stderr, flush=True)

# Protects all stdout writes (main request-handling thread + poll thread).
_stdout_lock = threading.Lock()

# ── Push notifications ─────────────────────────────────────────────────────────

def _emit_notification(method: str, params: dict) -> None:
    """Write a JSON-RPC notification (no id) to stdout."""
    msg = json.dumps({"jsonrpc": "2.0", "method": method, "params": params})
    with _stdout_lock:
        sys.stdout.write(msg + "\n")
        sys.stdout.flush()


# State for incremental polling via the Gmail History API.
_last_history_id: str | None = None
_poll_thread: threading.Thread | None = None
_POLL_INTERVAL_SECS = 60


def _start_polling() -> None:
    """Build the service eagerly, record the initial historyId, start poll thread."""
    global _last_history_id, _poll_thread
    svc = _get_service()
    if svc is None:
        log("Gmail push polling disabled: service not available.")
        return
    try:
        profile = _call(lambda: svc.users().getProfile(userId="me").execute(), "Gmail")
        _last_history_id = str(profile.get("historyId", ""))
        log(f"Gmail polling started (historyId={_last_history_id}, interval={_POLL_INTERVAL_SECS}s).")
    except Exception as e:
        log(f"Failed to get initial historyId, polling disabled: {_format_google_error(e, 'Gmail')}")
        return
    _poll_thread = threading.Thread(target=_poll_loop, daemon=True, name="gmail-poll")
    _poll_thread.start()


def _poll_loop() -> None:
    while True:
        time.sleep(_POLL_INTERVAL_SECS)
        _poll_once()


def _poll_once() -> None:
    global _last_history_id
    svc = _get_service()
    if svc is None or not _last_history_id:
        return
    try:
        result = _call(lambda: svc.users().history().list(
            userId="me",
            startHistoryId=_last_history_id,
            labelId="INBOX",
            historyTypes=["messageAdded"],
        ).execute(), "Gmail")

        # Always advance the cursor, even if no new messages.
        new_history_id = result.get("historyId")
        if new_history_id:
            _last_history_id = str(new_history_id)

        for record in result.get("history", []):
            for added in record.get("messagesAdded", []):
                msg_stub = added.get("message", {})
                if "INBOX" not in msg_stub.get("labelIds", []):
                    continue
                msg_id = msg_stub.get("id")
                if not msg_id:
                    continue
                _fetch_and_emit_email(svc, msg_id)

    except Exception as e:
        log(f"Gmail history poll error: {_format_google_error(e, 'Gmail')}")


def _fetch_and_emit_email(svc: Any, msg_id: str) -> None:
    """Fetch metadata for a message and emit an event/new_email notification."""
    try:
        msg = _call(lambda: svc.users().messages().get(
            userId="me",
            id=msg_id,
            format="metadata",
            metadataHeaders=["Subject", "From", "Date"],
        ).execute(), "Gmail")
        headers = {h["name"]: h["value"] for h in msg.get("payload", {}).get("headers", [])}
        _emit_notification("event/new_email", {
            "message_id": msg_id,
            "thread_id":  msg.get("threadId"),
            "subject":    headers.get("Subject", "(no subject)"),
            "from":       headers.get("From", "?"),
            "date":       headers.get("Date", "?"),
            "snippet":    msg.get("snippet", "")[:300],
        })
        log(f"Notification emitted: new email {msg_id} from {headers.get('From', '?')!r}")
    except Exception as e:
        log(f"Failed to fetch metadata for message {msg_id}: {_format_google_error(e, 'Gmail')}")


# ── Credentials / service ──────────────────────────────────────────────────────

_service = None
_creds = None
_creds_path: str | None = None
_init_error: str | None = None


def _persist_creds() -> None:
    """Write the current credentials back to disk (used after a token refresh)."""
    if _creds is not None and _creds_path:
        try:
            with open(_creds_path, "w") as f:
                f.write(_creds.to_json())
        except Exception as e:
            log(f"Could not persist refreshed credentials: {e}")


def _build_service() -> Any:
    """Build and return a Gmail service object, or None on failure."""
    global _init_error, _creds, _creds_path
    try:
        from google.auth.transport.requests import Request
        from google.oauth2.credentials import Credentials
        from googleapiclient.discovery import build
    except ImportError as e:
        _init_error = f"Missing dependencies: {e}. Install google-api-python-client and google-auth."
        log(_init_error)
        return None

    _creds_path = os.environ.get(
        "GMAIL_CREDS_PATH",
        os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "secrets", "gmail_creds.json"),
    )

    if not os.path.exists(_creds_path):
        _init_error = (
            f"Credentials file not found at {_creds_path}. "
            "Run scripts/gmail_oauth_setup.py first, or set GMAIL_CREDS_PATH."
        )
        log(_init_error)
        return None

    try:
        creds = Credentials.from_authorized_user_file(_creds_path)
    except Exception as e:
        _init_error = f"Failed to load credentials from {_creds_path}: {e}"
        log(_init_error)
        return None

    # Publish creds globally so _persist_creds / _call can see them.
    _creds = creds

    # Auto-refresh if expired; fail hard if we cannot refresh (need re-auth).
    try:
        if not creds.valid:
            if creds.expired and creds.refresh_token:
                creds.refresh(Request())
                _persist_creds()
                log("Token refreshed and saved.")
            else:
                _init_error = "Credentials invalid and cannot be refreshed. Re-run scripts/gmail_oauth_setup.py."
                log(_init_error)
                return None
    except Exception as e:
        _init_error = f"Failed to refresh credentials: {e}"
        log(_init_error)
        return None

    try:
        service = build("gmail", "v1", credentials=creds)
    except Exception as e:
        _init_error = f"Failed to build Gmail service: {e}"
        log(_init_error)
        return None

    log(f"Gmail service built successfully (creds: {_creds_path})")
    return service


def _get_service() -> Any:
    global _service
    if _service is None:
        _service = _build_service()
    return _service


# ── Error mapping & refresh-on-auth-error ──────────────────────────────────────


def _is_auth_error(e: Exception) -> bool:
    """True for 401 HttpError / RefreshError — candidates for a refresh+retry."""
    try:
        from googleapiclient.errors import HttpError
    except ImportError:
        return False
    if isinstance(e, HttpError):
        return getattr(e, "status_code", None) == 401
    try:
        from google.auth.exceptions import RefreshError
    except ImportError:
        return False
    return isinstance(e, RefreshError)


def _call(fn: Callable[[], Any], api_label: str) -> Any:
    """Run a googleapiclient call with one refresh-on-auth-error retry.

    If the access token expired mid-session the first call raises a 401 HttpError
    or a RefreshError. We refresh once, persist the new token, and retry the call.
    Anything else (or a second failure) is re-raised so the caller can format it
    via _format_google_error.
    """
    try:
        return fn()
    except Exception as e:
        if not _is_auth_error(e) or _creds is None or not getattr(_creds, "refresh_token", None):
            raise
        try:
            from google.auth.transport.requests import Request
            _creds.refresh(Request())
            _persist_creds()
            log("Access token refreshed mid-session after auth error; retrying the call.")
        except Exception as refresh_err:
            log(f"Mid-session token refresh failed: {refresh_err}")
            raise
        return fn()


def _http_error_reason(e: Exception) -> str:
    """Best-effort short reason string from an HttpError (for 400/4xx detail)."""
    return str(e).strip().replace("\n", " ")[:200]


def _format_google_error(e: Exception, api_label: str) -> str:
    """Map a googleapiclient / google-auth exception into an actionable Error: string."""
    try:
        from googleapiclient.errors import HttpError
    except ImportError:
        HttpError = None  # type: ignore
    try:
        from google.auth.exceptions import RefreshError
    except ImportError:
        RefreshError = None  # type: ignore

    if RefreshError is not None and isinstance(e, RefreshError):
        return (
            f"Error: {api_label} API token refresh failed (the refresh token may have been revoked "
            "or expired). Re-run scripts/gmail_oauth_setup.py to re-authenticate."
        )

    if HttpError is not None and isinstance(e, HttpError):
        status = getattr(e, "status_code", None)
        if status == 401:
            return (
                f"Error: {api_label} API rejected the access token (401). The OAuth token is invalid "
                "or revoked. Re-run scripts/gmail_oauth_setup.py to re-authenticate."
            )
        if status == 403:
            return (
                f"Error: {api_label} API returned 403 Forbidden. The OAuth scopes granted are "
                "insufficient for this operation, or the Gmail API is disabled in the Google Cloud "
                "Console. Verify the scopes in scripts/gmail_oauth_setup.py and the API enablement."
            )
        if status == 404:
            return (
                f"Error: {api_label} API returned 404 Not Found. Check the message/thread/attachment ID."
            )
        if status == 429:
            return f"Error: {api_label} API rate limit exceeded (429). Wait a moment and retry."
        if status == 400:
            return (
                f"Error: {api_label} API rejected the request as invalid (400). Check the parameters. "
                f"Detail: {_http_error_reason(e)}"
            )
        if status is not None and 500 <= status < 600:
            return f"Error: {api_label} API returned a server error (HTTP {status}). Retry in a moment."
        return f"Error: {api_label} API call failed (HTTP {status}). Detail: {_http_error_reason(e)}"

    return f"Error: {api_label} API call failed: {e}"


def _status_report(icon: str, label: str, kind: str, description: str, steps: list[str] | None = None) -> str:
    lines = [f"Status: {label} {icon} ({kind})", description]
    if steps:
        lines.append("")
        lines.append("What to do:")
        for i, s in enumerate(steps, 1):
            lines.append(f"{i}. {s}")
    return "\n".join(lines)


# ── Helpers ────────────────────────────────────────────────────────────────────


def _decode_body(parts: Any, mime_type: str = "text/plain") -> str:
    """Recursively extract the body of the given MIME type from MIME parts."""
    if isinstance(parts, list):
        for part in parts:
            if part.get("mimeType", "") == mime_type:
                data = part.get("body", {}).get("data", "")
                if data:
                    return _safe_b64decode(data)
            if "parts" in part:
                result = _decode_body(part["parts"], mime_type)
                if result:
                    return result
    return ""


def _safe_b64decode(data: str) -> str:
    """Decode URL-safe base64 to string."""
    try:
        # Add padding if needed.
        padded = data + "=" * (4 - len(data) % 4) if len(data) % 4 else data
        decoded = base64.urlsafe_b64decode(padded)
        return decoded.decode("utf-8", errors="replace")
    except Exception:
        return "(unable to decode)"


class _HTMLTextExtractor(HTMLParser):
    """Collect readable text from HTML, skipping scripts/styles and adding
    newlines around block-level tags. convert_charrefs=True unescapes entities."""

    _SKIP = {"script", "style", "head"}
    _BLOCK = {"p", "div", "br", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6"}

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self._chunks: list[str] = []
        self._skip_depth = 0

    def handle_starttag(self, tag: str, attrs: Any) -> None:
        if tag in self._SKIP:
            self._skip_depth += 1
        elif tag == "br":
            self._chunks.append("\n")

    def handle_endtag(self, tag: str) -> None:
        if tag in self._SKIP and self._skip_depth:
            self._skip_depth -= 1
        elif tag in self._BLOCK:
            self._chunks.append("\n")

    def handle_data(self, data: str) -> None:
        if not self._skip_depth:
            self._chunks.append(data)

    def get_text(self) -> str:
        return re.sub(r"\n{3,}", "\n\n", "".join(self._chunks)).strip()


def _html_to_text(html_str: str) -> str:
    """Convert an HTML email body to readable plain text (stdlib only)."""
    try:
        parser = _HTMLTextExtractor()
        parser.feed(html_str)
        return parser.get_text()
    except Exception:
        return html_str


def _format_datetime(ts_millis: int | None) -> str:
    """Format a unix timestamp in milliseconds to ISO-like string."""
    if ts_millis is None:
        return "?"
    return time.strftime("%Y-%m-%d %H:%M:%S", time.gmtime(ts_millis / 1000))


def _format_message_summary(msg: dict) -> str:
    """Format a message object (from list with metadata) into a summary line."""
    mid = msg.get("id", "?")
    headers = {h["name"]: h["value"] for h in msg.get("payload", {}).get("headers", [])}
    # When listing with metadata, headers might be elsewhere.
    payload = msg.get("payload", {})
    if not headers:
        headers = {h["name"]: h["value"] for h in payload.get("headers", [])}
    thread_id = msg.get("threadId", "?")
    subject = headers.get("Subject", "(no subject)")
    sender = headers.get("From", "?")
    date = headers.get("Date", "?")
    snippet = msg.get("snippet", "")[:80]
    return f"- {subject}\n  From: {sender} | Date: {date} | ID: {mid} | Thread: {thread_id}\n  {snippet}"


def _collect_attachments(parts: Any, results: list) -> None:
    """Recursively collect attachment filenames and IDs from MIME parts."""
    if not parts:
        return
    for part in parts:
        filename = part.get("filename", "")
        attachment_id = part.get("body", {}).get("attachmentId", "")
        if filename and attachment_id:
            results.append({"filename": filename, "attachmentId": attachment_id})
        if "parts" in part:
            _collect_attachments(part["parts"], results)


# ── Tool implementations ───────────────────────────────────────────────────────


def _gmail_status(args: dict | None = None) -> str:
    """Self-check: credentials load, the token refreshes when needed, and the API answers.

    Performs one cheap users().getProfile(userId='me') probe so we exercise the
    OAuth token, the network, and the Gmail API in a single call.
    """
    # Step 1: deps + creds file + service build.
    svc = _get_service()
    if svc is None:
        return _status_report("❌", "NOT_CONFIGURED", "action needed",
            f"The Gmail service could not be built: {_init_error or 'unknown error'}.",
            ["Run scripts/gmail_oauth_setup.py to authenticate and create secrets/gmail_creds.json.",
             "Or set the GMAIL_CREDS_PATH env var to point at an existing credentials file."])

    # Step 2: live probe — refresh-on-auth-error is handled inside _call.
    try:
        profile = _call(lambda: svc.users().getProfile(userId="me").execute(), "Gmail")
    except Exception as e:
        return _status_report("❌", "AUTH_OR_API_ERROR", "action needed",
            f"The Gmail API did not respond to the probe call: {_format_google_error(e, 'Gmail')}",
            ["Run scripts/gmail_oauth_setup.py to refresh / re-issue credentials.",
             "If credentials are valid, verify the Gmail API is enabled in the Google Cloud Console."])

    email = profile.get("emailAddress", "?")
    return _status_report("✅", "READY", "ok",
        "Google Gmail integration is operational: credentials load, the access token refreshes "
        "automatically, and the Gmail API responds. All tools (list/get/thread/labels/modify/send/"
        "download) are usable.\n"
        f"Account: {email}")


def _gmail_list_messages(args: dict) -> str:
    """List messages with optional filters."""
    svc = _get_service()
    if svc is None:
        return f"Error: {_init_error}"

    query = args.get("query", "")
    max_results = min(args.get("max_results", 20), 50)
    label_ids = args.get("label_ids")
    page_token = args.get("page_token")

    params: dict = {
        "userId": "me",
        "maxResults": max_results,
    }
    if query:
        params["q"] = query
    if label_ids:
        if isinstance(label_ids, str):
            label_ids = [label_ids]
        params["labelIds"] = label_ids
    if page_token:
        params["pageToken"] = page_token

    try:
        result = _call(lambda: svc.users().messages().list(**params).execute(), "Gmail")
    except Exception as e:
        return _format_google_error(e, "Gmail")

    items = result.get("messages", [])
    if not items:
        return "No messages found."

    # Fetch full metadata for each message.
    lines = [f"Messages ({len(items)} total):"]
    for entry in items:
        try:
            msg = _call(lambda e=entry: svc.users().messages().get(
                userId="me", id=e["id"], format="metadata",
                metadataHeaders=["Subject", "From", "Date"],
            ).execute(), "Gmail")
            lines.append(_format_message_summary(msg))
        except Exception as e:
            lines.append(f"- {entry['id']} (error fetching: {_http_error_reason(e)})")

    # Add paging info.
    next_token = result.get("nextPageToken")
    if next_token:
        lines.append(f"\nMore results available. Use page_token='{next_token}' to get next page.")

    return "\n".join(lines)


def _gmail_get_message(args: dict) -> str:
    """Get full content of a single message by ID."""
    svc = _get_service()
    if svc is None:
        return f"Error: {_init_error}"

    msg_id = args.get("message_id")
    if not msg_id:
        return "Error: Missing required parameter 'message_id'."
    include_body = args.get("include_body", True)

    fmt = "full" if include_body else "metadata"
    meta_headers = [] if include_body else ["Subject", "From", "To", "Date"]

    try:
        msg = _call(lambda: svc.users().messages().get(
            userId="me", id=msg_id, format=fmt,
            **({"metadataHeaders": meta_headers} if meta_headers else {}),
        ).execute(), "Gmail")
    except Exception as e:
        return _format_google_error(e, "Gmail")

    payload = msg.get("payload", {})
    headers = {h["name"]: h["value"] for h in payload.get("headers", [])}

    lines = [
        f"ID: {msg.get('id', '?')}",
        f"Thread: {msg.get('threadId', '?')}",
        f"From: {headers.get('From', '?')}",
        f"To: {headers.get('To', '?')}",
        f"Date: {headers.get('Date', '?')}",
        f"Subject: {headers.get('Subject', '(no subject)')}",
        f"Labels: {', '.join(msg.get('labelIds', []))}",
    ]

    # List attachment filenames (needs the full payload). Download them via
    # download_attachments, which only needs this message_id.
    attachments: list = []
    _collect_attachments(payload.get("parts", []), attachments)
    if attachments:
        lines.append(f"Attachments: {', '.join(a['filename'] for a in attachments)}")

    if include_body:
        parts = payload.get("parts", [])
        body_text = _decode_body(parts)  # prefer text/plain
        body_label = "--- Body ---"
        if not body_text:
            # Fall back to the HTML part, converted to readable text.
            html_body = _decode_body(parts, "text/html")
            if html_body:
                body_text = _html_to_text(html_body)
                body_label = "--- Body (converted from HTML) ---"
        if not body_text:
            # Single-part message: the body lives inline on the payload.
            body_data = payload.get("body", {}).get("data", "")
            if body_data:
                decoded = _safe_b64decode(body_data)
                if payload.get("mimeType", "") == "text/html":
                    body_text = _html_to_text(decoded)
                    body_label = "--- Body (converted from HTML) ---"
                else:
                    body_text = decoded
        if body_text:
            lines.append(f"\n{body_label}")
            # Truncate very long bodies.
            if len(body_text) > 10000:
                lines.append(body_text[:10000] + "\n... [truncated at 10000 chars]")
            else:
                lines.append(body_text)
        else:
            lines.append("\n(no text body found)")

    return "\n".join(lines)


def _gmail_get_thread(args: dict) -> str:
    """Get an entire thread (all messages in it)."""
    svc = _get_service()
    if svc is None:
        return f"Error: {_init_error}"

    thread_id = args.get("thread_id")
    if not thread_id:
        return "Error: Missing required parameter 'thread_id'."

    try:
        thread = _call(lambda: svc.users().threads().get(
            userId="me", id=thread_id, format="metadata",
            metadataHeaders=["Subject", "From", "Date"],
        ).execute(), "Gmail")
    except Exception as e:
        return _format_google_error(e, "Gmail")

    messages = thread.get("messages", [])
    subject = ""
    lines = [f"Thread: {thread_id} ({len(messages)} messages)"]
    for i, msg in enumerate(messages, 1):
        headers = {h["name"]: h["value"] for h in msg.get("payload", {}).get("headers", [])}
        if not subject:
            subject = headers.get("Subject", "(no subject)")
        lines.append(f"\n[{i}] From: {headers.get('From', '?')} | Date: {headers.get('Date', '?')}")
        lines.append(f"    ID: {msg.get('id', '?')}")
        snippet = msg.get("snippet", "")
        if snippet:
            lines.append(f"    {snippet[:200]}")

    if subject:
        lines.insert(1, f"Subject: {subject}")

    return "\n".join(lines)


def _gmail_list_labels(args: dict) -> str:
    """List all labels/categories in the Gmail account."""
    svc = _get_service()
    if svc is None:
        return f"Error: {_init_error}"

    try:
        result = _call(lambda: svc.users().labels().list(userId="me").execute(), "Gmail")
    except Exception as e:
        return _format_google_error(e, "Gmail")

    items = result.get("labels", [])
    if not items:
        return "No labels found."

    lines = ["Labels:"]
    for lbl in items:
        lid = lbl.get("id", "?")
        name = lbl.get("name", "?")
        label_type = lbl.get("type", "?")
        msg_count = lbl.get("messagesTotal", "?")
        unread = lbl.get("messagesUnread", 0)
        lines.append(f"- {name} ({lid}) [{label_type}] — {msg_count} total, {unread} unread")

    return "\n".join(lines)


def _gmail_modify_message(args: dict) -> str:
    """Modify message labels (add/remove labels, mark read/archive/star)."""
    svc = _get_service()
    if svc is None:
        return f"Error: {_init_error}"

    msg_id = args.get("message_id")
    if not msg_id:
        return "Error: Missing required parameter 'message_id'."

    add_labels = args.get("add_labels", [])
    remove_labels = args.get("remove_labels", [])

    if isinstance(add_labels, str):
        add_labels = [add_labels]
    if isinstance(remove_labels, str):
        remove_labels = [remove_labels]

    body: dict = {}
    if add_labels:
        body["addLabelIds"] = add_labels
    if remove_labels:
        body["removeLabelIds"] = remove_labels

    try:
        _call(lambda: svc.users().messages().modify(userId="me", id=msg_id, body=body).execute(), "Gmail")
    except Exception as e:
        return _format_google_error(e, "Gmail")

    changes = []
    if add_labels:
        changes.append(f"added labels: {add_labels}")
    if remove_labels:
        changes.append(f"removed labels: {remove_labels}")
    return f"✅ Message {msg_id} modified: {'; '.join(changes)}"


# messages.send embeds the whole message as base64 inside the JSON request body;
# Gmail caps that request around 35 MB and base64 inflates the payload ~33%, so we
# reject well before the ceiling with a clear error instead of an opaque 400.
_MAX_ATTACHMENT_BYTES = 25 * 1024 * 1024


def _gmail_send_message(args: dict) -> str:
    """Send an email message, optionally with one or more file attachments."""
    svc = _get_service()
    if svc is None:
        return f"Error: {_init_error}"

    to = args.get("to")
    subject = args.get("subject", "")
    body_text = args.get("body", "")
    cc = args.get("cc")
    bcc = args.get("bcc")
    in_reply_to = args.get("in_reply_to")
    thread_id = args.get("thread_id")
    attachments = args.get("attachments") or []

    if not to:
        return "Error: Missing required parameter 'to'."

    # Accept a single path or an array (mirrors add_labels / remove_labels).
    if isinstance(attachments, str):
        attachments = [attachments]

    # Resolve every attachment path (relative paths are anchored at the project
    # root, like _build_service / download_attachments) and fail early if any file
    # is missing — the email is NOT sent unless every attachment is present.
    project_root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    resolved: list[str] = []
    total_bytes = 0
    for raw_path in attachments:
        path = raw_path if os.path.isabs(raw_path) else os.path.join(project_root, raw_path)
        if not os.path.isfile(path):
            return f"Error: attachment not found: {raw_path}"
        total_bytes += os.path.getsize(path)
        resolved.append(path)

    if total_bytes > _MAX_ATTACHMENT_BYTES:
        return (
            f"Error: attachments total ~{total_bytes // (1024 * 1024)} MB, which exceeds the "
            f"{_MAX_ATTACHMENT_BYTES // (1024 * 1024)} MB send limit. Send fewer or smaller files."
        )

    # EmailMessage builds a plain text/plain message when there are no attachments
    # and switches to multipart/mixed automatically once one is added.
    msg = EmailMessage()
    msg["To"] = to
    if cc:
        msg["Cc"] = cc
    if bcc:
        msg["Bcc"] = bcc
    msg["Subject"] = subject
    # RFC 2822 threading headers for in-thread reply.
    if in_reply_to:
        msg["In-Reply-To"] = f"<{in_reply_to}>"
        msg["References"] = f"<{in_reply_to}>"
    msg.set_content(body_text)

    for path in resolved:
        ctype, encoding = mimetypes.guess_type(path)
        # Fall back to a generic binary type for unknown or compressed files.
        if ctype is None or encoding is not None:
            ctype = "application/octet-stream"
        maintype, subtype = ctype.split("/", 1)
        try:
            with open(path, "rb") as f:
                data = f.read()
        except Exception as e:
            return f"Error: could not read attachment {path}: {e}"
        msg.add_attachment(data, maintype=maintype, subtype=subtype,
                           filename=os.path.basename(path))

    encoded = base64.urlsafe_b64encode(msg.as_bytes()).decode("utf-8")

    # Build API body — include threadId when replying in-thread.
    api_body: dict = {"raw": encoded}
    if thread_id:
        api_body["threadId"] = thread_id

    try:
        sent = _call(lambda: svc.users().messages().send(userId="me", body=api_body).execute(), "Gmail")
    except Exception as e:
        return _format_google_error(e, "Gmail")

    suffix = f" ({len(resolved)} attachment{'s' if len(resolved) != 1 else ''})" if resolved else ""
    return f"✅ Message sent! ID: {sent.get('id', '?')}{suffix}"


def _gmail_get_profile(args: dict) -> str:
    """Get Gmail profile info (email address, total/thread count)."""
    svc = _get_service()
    if svc is None:
        return f"Error: {_init_error}"

    try:
        profile = _call(lambda: svc.users().getProfile(userId="me").execute(), "Gmail")
    except Exception as e:
        return _format_google_error(e, "Gmail")

    return (
        f"Email: {profile.get('emailAddress', '?')}\n"
        f"Messages total: {profile.get('messagesTotal', '?')}\n"
        f"Threads total: {profile.get('threadsTotal', '?')}\n"
        f"History ID: {profile.get('historyId', '?')}"
    )


def _gmail_create_label(args: dict) -> str:
    """Create a new Gmail label."""
    svc = _get_service()
    if svc is None:
        return f"Error: {_init_error}"

    name = args.get("name")
    if not name:
        return "Error: Missing required parameter 'name'."

    label_list_visibility = args.get("label_list_visibility", "labelShow")
    message_list_visibility = args.get("message_list_visibility", "show")

    body = {
        "name": name,
        "labelListVisibility": label_list_visibility,
        "messageListVisibility": message_list_visibility,
    }

    try:
        result = _call(lambda: svc.users().labels().create(userId="me", body=body).execute(), "Gmail")
    except Exception as e:
        return _format_google_error(e, "Gmail")

    return f"✅ Label '{result.get('name', name)}' created (ID: {result.get('id', '?')})"


def _gmail_download_attachments(args: dict) -> str:
    """Download all attachments from a Gmail message to a local folder."""
    svc = _get_service()
    if svc is None:
        return f"Error: {_init_error}"

    msg_id = args.get("message_id")
    if not msg_id:
        return "Error: Missing required parameter 'message_id'."

    # Default to data/gmail_attachments/ (served via /data/... in the frontend,
    # consistent with whatsapp_media). Allow override.
    default_folder = os.path.join(
        os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
        "data", "gmail_attachments",
    )
    folder = args.get("folder") or default_folder

    try:
        msg = _call(lambda: svc.users().messages().get(userId="me", id=msg_id, format="full").execute(), "Gmail")
    except Exception as e:
        return _format_google_error(e, "Gmail")

    payload = msg.get("payload", {})

    attachments: list = []
    _collect_attachments(payload.get("parts", []), attachments)

    if not attachments:
        return "No attachments found."

    os.makedirs(folder, exist_ok=True)

    saved = []
    for att in attachments:
        filename = att["filename"]
        attachment_id = att["attachmentId"]

        try:
            result = _call(lambda a=att: svc.users().messages().attachments().get(
                userId="me", messageId=msg_id, id=a["attachmentId"],
            ).execute(), "Gmail")
        except Exception as e:
            saved.append(f"- {filename}: ERROR fetching attachment: {_http_error_reason(e)}")
            continue

        data = result.get("data", "")
        if not data:
            saved.append(f"- {filename}: empty attachment data")
            continue

        try:
            file_data = base64.urlsafe_b64decode(data)
        except Exception as e:
            saved.append(f"- {filename}: ERROR decoding: {e}")
            continue

        safe_name = os.path.basename(filename)
        file_path = os.path.join(folder, safe_name)

        try:
            with open(file_path, "wb") as f:
                f.write(file_data)
        except Exception as e:
            saved.append(f"- {safe_name}: ERROR writing file: {e}")
            continue

        abs_path = os.path.abspath(file_path)
        size = len(file_data)
        saved.append(f"- {abs_path} ({size} bytes)")

    return "\n".join(["✅ Attachments downloaded:"] + saved)


# ── Tool manifest ──────────────────────────────────────────────────────────────

TOOLS = [
    {
        "name": "status",
        "description": (
            "Self-check that the Google Gmail integration is operational: verifies the OAuth "
            "credentials load, the access token refreshes when needed, and the Gmail API responds, "
            "by performing one cheap getProfile probe. Call this first whenever another gmail tool "
            "fails, or to give the user a quick yes/no on whether Gmail is usable right now."
        ),
        "inputSchema": {"type": "object", "properties": {}},
    },
    {
        "name": "list_messages",
        "description": (
            "List Gmail messages with optional query and label filter. Returns summaries with "
            "subject, sender, date, message ID and thread ID. Use Gmail search syntax in 'query' "
            "(e.g. 'from:john', 'is:unread', 'after:2024/01/01', 'has:attachment'). Pass the "
            "returned IDs to get_message / modify_message."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Gmail search query (e.g. 'from:john', 'is:unread', 'after:2024/01/01'). Leave empty for all recent messages.",
                },
                "max_results": {
                    "type": "integer",
                    "description": "Max messages to return (default 20, max 50).",
                },
                "label_ids": {
                    "type": ["string", "array"],
                    "items": {"type": "string"},
                    "description": "Filter by label IDs (e.g. 'INBOX', or ['INBOX','STARRED']). Pass a single string or an array.",
                },
                "page_token": {
                    "type": "string",
                    "description": "Opaque token from a previous response's 'More results available' line, to fetch the next page.",
                },
            },
        },
    },
    {
        "name": "get_message",
        "description": (
            "Get full content of a Gmail message by ID, including body text (truncated at 10000 "
            "chars). HTML-only emails are converted to readable text, and any attachment filenames "
            "are listed (download them with download_attachments)."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "message_id": {
                    "type": "string",
                    "description": "The Gmail message ID to retrieve.",
                },
                "include_body": {
                    "type": "boolean",
                    "description": "Whether to include the full body text (default true).",
                },
            },
            "required": ["message_id"],
        },
    },
    {
        "name": "get_thread",
        "description": "Get all messages in a thread by thread ID, newest last.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "thread_id": {
                    "type": "string",
                    "description": "The Gmail thread ID to retrieve.",
                },
            },
            "required": ["thread_id"],
        },
    },
    {
        "name": "list_labels",
        "description": "List all Gmail labels/folders/categories with total and unread message counts. Use to resolve label IDs for modify_message.",
        "inputSchema": {
            "type": "object",
            "properties": {},
        },
    },
    {
        "name": "modify_message",
        "description": (
            "Modify message labels: mark read, archive, star, etc. Use label IDs like 'UNREAD', "
            "'STARRED', 'INBOX'. remove_labels=['UNREAD'] marks as read; remove_labels=['INBOX'] "
            "archives. add_labels/remove_labels each accept a single string or an array."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "message_id": {
                    "type": "string",
                    "description": "The Gmail message ID to modify.",
                },
                "add_labels": {
                    "type": ["string", "array"],
                    "items": {"type": "string"},
                    "description": "Label ID(s) to add (e.g. 'STARRED', or ['STARRED','IMPORTANT']).",
                },
                "remove_labels": {
                    "type": ["string", "array"],
                    "items": {"type": "string"},
                    "description": "Label ID(s) to remove (e.g. 'UNREAD' to mark as read, 'INBOX' to archive).",
                },
            },
            "required": ["message_id"],
        },
    },
    {
        "name": "send_message",
        "description": (
            "Send an email via Gmail. Supports in-thread replies via the optional in_reply_to "
            "(message ID) and thread_id parameters. For a reply, pass both for correct threading "
            "across email clients. Attach files by passing local file paths in 'attachments'; the "
            "server reads them from disk and, if any path does not exist, the email is NOT sent and "
            "an error is returned."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "to": {
                    "type": "string",
                    "description": "Recipient email address.",
                },
                "subject": {
                    "type": "string",
                    "description": "Email subject line.",
                },
                "body": {
                    "type": "string",
                    "description": "Plain text body of the email.",
                },
                "cc": {
                    "type": "string",
                    "description": "CC recipient email (optional).",
                },
                "bcc": {
                    "type": "string",
                    "description": "BCC recipient email (optional).",
                },
                "in_reply_to": {
                    "type": "string",
                    "description": "Message ID to reply to (adds In-Reply-To and References headers for proper threading).",
                },
                "thread_id": {
                    "type": "string",
                    "description": "Thread ID to attach the reply to (ensures the message appears in the correct Gmail thread).",
                },
                "attachments": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": (
                        "File path(s) to attach. Each is absolute, or relative to the project root "
                        "(e.g. 'data/gmail_attachments/report.pdf'). The server reads each file from "
                        "disk; if any path does not exist the email is NOT sent and an error is "
                        "returned. Total size limit ~25 MB."
                    ),
                },
            },
            "required": ["to", "subject", "body"],
        },
    },
    {
        "name": "get_profile",
        "description": "Get Gmail profile info: email address, total message/thread count, current history ID.",
        "inputSchema": {
            "type": "object",
            "properties": {},
        },
    },
    {
        "name": "create_label",
        "description": "Create a new Gmail label/folder. Returns the new label ID. Fails if a label with the same name already exists.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the new label.",
                },
                "label_list_visibility": {
                    "type": "string",
                    "description": "Visibility in the label list: 'labelShow' (default), 'labelShowIfUnread', 'labelHide'.",
                    "default": "labelShow",
                },
                "message_list_visibility": {
                    "type": "string",
                    "description": "Visibility in the message list: 'show' (default) or 'hide'.",
                    "default": "show",
                },
            },
            "required": ["name"],
        },
    },
    {
        "name": "download_attachments",
        "description": (
            "Download all attachments from a Gmail message to a local folder. "
            "Defaults to data/gmail_attachments/ (served via /data/... in the frontend). "
            "Returns the absolute path and size of each saved file."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "message_id": {
                    "type": "string",
                    "description": "The Gmail message ID to download attachments from.",
                },
                "folder": {
                    "type": "string",
                    "description": "Local folder to save attachments into (default: data/gmail_attachments/).",
                },
            },
            "required": ["message_id"],
        },
    },
]


# ── JSON-RPC dispatch ──────────────────────────────────────────────────────────

TOOL_DISPATCH = {
    "status":              _gmail_status,
    "list_messages":       _gmail_list_messages,
    "get_message":         _gmail_get_message,
    "get_thread":          _gmail_get_thread,
    "list_labels":         _gmail_list_labels,
    "modify_message":      _gmail_modify_message,
    "send_message":        _gmail_send_message,
    "get_profile":         _gmail_get_profile,
    "create_label":        _gmail_create_label,
    "download_attachments": _gmail_download_attachments,
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
                "name": "gmail",
                "version": "0.2.0",
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
    log("Starting Gmail MCP server")
    # Build the service eagerly and start the background polling thread.
    _start_polling()
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
                with _stdout_lock:
                    sys.stdout.write(resp + "\n")
                    sys.stdout.flush()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
