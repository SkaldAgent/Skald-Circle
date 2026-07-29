# Mobile Connector

- **Plugin id:** `mobile-connector`
- **Category:** Mobile companion app bridge
- **Runs:** connects out to a relay server over an end-to-end encrypted WebSocket — needs internet access, but never exposes this machine directly to the internet

## What it does

Bridges the assistant's Inbox — pending approvals, clarification questions, and MCP elicitations — to a companion mobile app on the user's phone, end-to-end encrypted so even the relay server can't read the content. The phone can also show the full web app UI over the same encrypted tunnel, without any port-forwarding or the server being reachable from the internet.

Unlike most plugins, per-user access here is **not** the usual grant checklist — it's the device↔user binding itself (see pairing below). So this plugin is deliberately absent from the **Plugins** list on a user's page: there is no box to tick, and pairing a device is what grants access.

## The Mobile App page

Everything lives in one sidebar page — **Mobile App** (`#plugin/mobile-connector/app`), visible to every logged-in user:

- A **connection status** pill at the top (connected / connecting / not running). When the connection is down, the last connection error is shown to help troubleshooting.
- The **device list**: an admin sees every paired device (and can reassign or revoke any of them); anyone else sees only their own devices and can revoke them.
- **Pair new device** (top-right): opens the pairing dialog with the QR code.
- **Settings** (gear icon, admin only): opens the plugin's configuration dialog. This plugin's settings live *here*, not in the generic plugin configuration page.

## Configuring (admin)

Open the settings dialog from the Mobile App page (gear icon). Fields:

- **Relay server** — pick *SkaldCircle — Test Server*, or *Custom* to enter any `wss://` URL by hand. (*SkaldCircle — Official Relay Server* is listed but not available yet.)
- **Pairing code lifetime** (default `300`, max `600`) — seconds a pairing QR code stays valid.
- **Require device confirmation** (default `true`, recommended) — a device paired outside a web pairing window (e.g. via the assistant) stays "pending" until an admin explicitly assigns it; don't turn this off without a good reason.
- **Notification delay** (default `20`) — grace period before pushing an approval/question to the phone, so answering on the computer first skips the redundant phone notification. `0` = push immediately. (Elicitations are always pushed immediately regardless of this setting.)

## Pairing a device (self-service)

1. Open the **Mobile App** page and click **Pair new device** — a dialog shows a QR code.
2. Scan it from the mobile app.
3. The device is automatically linked to *your* account and works immediately — the dialog confirms the pairing.

An admin can later reassign a device to another user from the device list.

## Notes

- A device stays bound until revoked from the Mobile App page (an admin can revoke any device; you can revoke your own).
- If a user asks "why isn't my phone getting notifications", check: the status pill on the Mobile App page is "Connected", their device is bound (not still pending), and — if it's not urgent — that they're not just inside the notification-delay grace window.
