# Remote Connectivity (Tailscale)

- **Plugin id:** `remote_connectivity` (package name `plugin-tailscale-remote` — the id differs from the package name)
- **Category:** Remote access
- **Runs:** on this machine; joins a [Tailscale](https://tailscale.com) mesh network

## What it does

Makes the web app reachable from other devices over a Tailscale mesh VPN — e.g. from a phone or laptop away from home — **without** port-forwarding the router or exposing anything to the public internet. Once running, the app is reachable at this device's Tailscale IP from any other device on the same tailnet.

This is a different, simpler style of remote access than the Mobile Connector plugin (which uses an external relay and works even without Tailscale installed anywhere).

## Requirements

Depends on which provider is chosen:

- **`tailscale_sys`** (default, recommended) — the host machine must already have the official Tailscale app/daemon installed and logged in. This plugin just reuses it.
- **`tailscale`** (experimental, embedded) — no separate install needed, but requires a Tailscale auth key generated from the Tailscale admin console.

## Enabling & configuring (admin)

1. Plugins page → **Remote Connectivity** → enable, then **Configure**.
2. Fields:
   - **`provider`** (`tailscale_sys` | `tailscale`, default `tailscale_sys`) — see requirements above.
   - **`auth_key`** — only for the embedded `tailscale` provider; a Tailscale auth key (`tskey-auth-…`), needed on first join.
   - **`hostname`** (default `personal-agent`) — only for the embedded provider; the name this node requests on the tailnet.
   - **`key_file`** (default `data/tailscale_keys.json`) — only for the embedded provider; where its node identity is persisted between restarts.

## Notes

- `tailscale_sys` is the recommended path for anyone who already uses Tailscale on their network — it's simpler and more reliable than the embedded mode.
- The embedded `tailscale` provider is explicitly marked experimental; prefer `tailscale_sys` unless there's a specific reason (e.g. not wanting a separate Tailscale install on the host) to use it.
