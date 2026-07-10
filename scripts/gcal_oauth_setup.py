#!/usr/bin/env python3
"""Generate a Google OAuth token for the Calendar API (read + write).

This script runs a local OAuth flow that:
1. Opens your browser automatically to the Google authorization page
2. Handles the callback via a local HTTP server
3. Saves the resulting token to ./secrets/google_creds.json

Required OAuth scope: https://www.googleapis.com/auth/calendar
(full access — needed for create, update, delete, respond).

No manual copy-paste required.
"""

from __future__ import annotations

import json
import os
import sys

SCOPES = [
    "https://www.googleapis.com/auth/calendar",
]

_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SECRET_PATH = os.path.join(_ROOT, "secrets", "google_creds.json")
_OAUTH_CLIENT_PATH = os.path.join(_ROOT, "secrets", "google_oauth_client.json")


def _load_oauth_client() -> tuple[str, str]:
    if not os.path.exists(_OAUTH_CLIENT_PATH):
        print(f"Missing OAuth client file: {_OAUTH_CLIENT_PATH}")
        print('Create it with: {"client_id": "...", "client_secret": "..."}')
        sys.exit(1)
    with open(_OAUTH_CLIENT_PATH) as f:
        data = json.load(f)
    return data["client_id"], data["client_secret"]


def main() -> None:
    try:
        from google.auth.transport.requests import Request
        from google.oauth2.credentials import Credentials
        from google_auth_oauthlib.flow import InstalledAppFlow
    except ImportError as e:
        print(f"Missing dependencies: {e}")
        print("Install with: pip install google-auth google-auth-oauthlib google-api-python-client")
        sys.exit(1)

    creds = None

    # Try to load existing credentials first.
    if os.path.exists(SECRET_PATH):
        print(f"Existing credentials found at {SECRET_PATH}")
        try:
            creds = Credentials.from_authorized_user_file(SECRET_PATH, SCOPES)
        except Exception:
            creds = None

    if creds and creds.valid:
        print("Credentials are already valid!")
        print(f"  Scopes: {creds.scopes}")
        return

    if creds and creds.expired and creds.refresh_token:
        print("Token expired. Attempting refresh...")
        try:
            creds.refresh(Request())
            print("Token refreshed successfully!")
        except Exception as e:
            print(f"Refresh failed: {e}")
            creds = None

    if not creds or not creds.valid:
        client_id, client_secret = _load_oauth_client()
        flow = InstalledAppFlow.from_client_config(
            {
                "installed": {
                    "client_id": client_id,
                    "client_secret": client_secret,
                    "auth_uri": "https://accounts.google.com/o/oauth2/auth",
                    "token_uri": "https://oauth2.googleapis.com/token",
                    "redirect_uris": ["http://localhost"],
                }
            },
            SCOPES,
        )

        print("\nOpening browser for Google authorization...")
        creds = flow.run_local_server(
            port=0,
            open_browser=True,
            prompt="consent",
            access_type="offline",
        )

    os.makedirs(os.path.dirname(SECRET_PATH), exist_ok=True)
    with open(SECRET_PATH, "w") as f:
        f.write(creds.to_json())

    print(f"\n✅ Google Calendar OAuth token saved to {SECRET_PATH}")
    print(f"   Scopes: {creds.scopes}")


if __name__ == "__main__":
    main()
