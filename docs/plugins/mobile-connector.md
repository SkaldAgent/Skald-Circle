# Mobile Connector

- **Plugin id:** `mobile-connector`
- **Category:** Mobile companion app bridge
- **Runs:** connects out to a relay server over an end-to-end encrypted WebSocket — needs internet access, but never exposes this machine directly to the internet

## What it does

Bridges the assistant's Inbox — pending approvals, clarification questions, and MCP elicitations — to a companion mobile app on the user's phone, end-to-end encrypted so even the relay server can't read the content. The phone can also show the full web app UI over the same encrypted tunnel, without any port-forwarding or the server being reachable from the internet.

Unlike most plugins, per-user access here is **not** the usual grant checklist — it's the device↔user binding itself (see pairing below), so this plugin doesn't show the normal "user access" list in the admin UI.

## Requirements

- A relay server URL (`wss://…`) to connect through.
- The companion mobile app installed on the user's phone.

## Enabling & configuring (admin)

1. Plugin catalog → **Mobile Connector** → enable, then **Configure**.
2. Fields:
   - **`relay_url`** (required) — the relay server's WebSocket URL.
   - **`pairing_ttl`** (default `300`, max `600`) — seconds a pairing QR code stays valid.
   - **`require_device_confirmation`** (default `true`, recommended) — a newly paired device stays "pending" until an admin explicitly authorizes it; don't turn this off without a good reason.
   - **`notify_delay_secs`** (default `20`) — grace period before pushing an approval/question to the phone, so answering on the computer first skips the redundant phone notification. `0` = push immediately. (Elicitations are always pushed immediately regardless of this setting.)

## Pairing a device (admin-mediated)

This is intentionally **not** self-service, unlike Telegram:

1. Admin opens **Pair a device** (sidebar, admin-only — `#plugin/mobile-connector/pairing`), which shows a QR code.
2. The user scans it from the mobile app.
3. The new device appears as "pending" on the **Mobile devices** page (`#plugin/mobile-connector/devices`).
4. The admin picks which user account to bind it to and confirms — only then can that device see that user's Inbox.

## Notes

- A device stays bound until an admin revokes it from the Mobile devices page.
- If a user asks "why isn't my phone getting notifications", check: the plugin is enabled, their device is bound (not still pending), and — if it's not urgent — that they're not just inside the `notify_delay_secs` grace window.
