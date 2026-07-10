# Remote Connectivity

Exposes the Skald web app on a private mesh network so remote clients (iOS app, NAS, etc.) can connect without port forwarding or internet exposure.

---

## Architecture

```
core-api                          plugin-tailscale-remote crate
─────────────────────────────     ──────────────────────────────────────────────
trait RemoteAccess            ←── RemotePlugin  (src/lib.rs)
(core_api::remote)                TailscaleSystemProvider  (src/tailscale_sys.rs)  ← default
                                  TailscaleEmbeddedProvider  (src/tailscale.rs)
                                  [feature: remote-tailscale]
```

- **`RemoteAccess` trait** (`core_api::remote`): vendor-agnostic interface. The core (`Skald`, WS handler, etc.) only knows this trait.
- **`TailscaleSystemProvider`** (`crates/plugin-tailscale-remote/src/tailscale_sys.rs`): **recommended provider**. Reads the mesh IP via `tailscale ip -4` (requires `tailscaled` running on the host). Binds a standard `tokio::net::TcpListener` — no experimental dependencies.
- **`TailscaleEmbeddedProvider`** (`crates/plugin-tailscale-remote/src/tailscale.rs`): embedded alternative using `tailscale-rs`. Feature-gated: `remote-tailscale`, **off by default** (enable via the root crate's `embedded-tailscale` feature). No system daemon required, but currently pre-1.0 with known DERP/reconnect issues, and it drags the `aws-lc-rs` C crypto build back into the tree (see [Feature Flag](#feature-flag)). Use only when a daemon cannot be installed (e.g. unrooted NAS).
- **`RemotePlugin`** (`crates/plugin-tailscale-remote/src/lib.rs`): wires the provider into the plugin lifecycle. Spawns a second Axum server on the mesh interface using the same router as the local server.

### `RemoteAccess` trait

```rust
pub trait RemoteAccess: Send + Sync {
    fn provider_name(&self) -> &str;
    async fn device_ip(&self) -> Result<Ipv4Addr>;
    fn is_connected(&self) -> bool;
    async fn shutdown(&self);
}
```

Stored in `Skald::remote: Arc<RwLock<Option<Arc<dyn RemoteAccess>>>>`. `None` when the plugin is disabled.

### Dual-bind strategy

The plugin (not the `WebServer`) is responsible for the mesh-facing server:

1. `RemotePlugin::start(state)` calls `extract_deps(state)` once, storing three named fields:
   - `port: u16` — TCP port to bind on the mesh interface
   - `remote_slot: Arc<RwLock<Option<Arc<dyn RemoteAccess>>>>` — slot in `Skald` to register the active provider
   - `router_factory: Arc<dyn Fn() -> Router + Send + Sync>` — closure that rebuilds the Axum router
2. The internal helpers (`start_tailscale_sys`, `start_tailscale`) use only those three fields — no `Arc<Skald>` reference after extraction.
3. `start_tailscale_sys` binds `tokio::net::TcpListener::bind((mesh_ip, port))`.
4. `start_tailscale` calls `provider.axum_listener(port)` → `tailscale::axum::Listener`.
5. Both call `router_factory()` to get a fresh router and spawn `axum::serve(listener, router)` guarded by a `CancellationToken`.

`extract_deps` uses `std::sync::OnceLock` — idempotent across `reload()` calls. The values are stable for the lifetime of the process (port and static dir come from config, the remote slot is the same `Arc`).

The local server on `127.0.0.1:PORT` is unaffected.

---

## Configuration

Config is stored in the **`plugins` SQLite table** (not in `config.yml`).

| Key | Type | Default | Description |
|---|---|---|---|
| `provider` | string | `"tailscale_sys"` | `tailscale_sys` (system daemon, recommended) or `tailscale` (embedded, no daemon). |
| `auth_key` | string | — | Tailscale auth key (`tskey-auth-...`). Only required for the `tailscale` embedded provider on first join. |
| `hostname` | string | `"personal-agent"` | Hostname on the tailnet. Only used by the embedded `tailscale` provider. |
| `key_file` | string | `"data/tailscale_keys.json"` | Path for persisting node identity between restarts. Only used by the embedded `tailscale` provider. |

Config is persisted automatically and survives restarts.

---

## Agent Workflow

### Using the system Tailscale daemon (recommended)

Requires Tailscale already installed and logged in on the host machine.

```
1. toggle_item(kind="plugin", id="remote_connectivity", enabled=true)
2. restart
→ On next boot: plugin reads IP from tailscale ip -4, mesh server starts, app reachable at <ts-ip>:3000
```

No auth key needed — the system daemon is already authenticated.

### Using the embedded provider (no daemon)

Requires a binary **built with `--features embedded-tailscale`** (off by default;
see [Feature Flag](#feature-flag)). Without it, selecting `provider="tailscale"`
fails at start with `unknown provider 'tailscale'`.

```
1. configure_plugin "remote_connectivity" {"provider":"tailscale","auth_key":"tskey-auth-...","hostname":"personal-agent"}
2. toggle_item(kind="plugin", id="remote_connectivity", enabled=true)
3. restart
→ On next boot: embedded tailscale connects, mesh server starts, app reachable at <ts-ip>:3000
```

After first setup, the plugin auto-starts on every boot (persisted `enabled=true` in DB).

To check status: `list_items` (type=plugins) → look for `remote_connectivity`, check `running` and `runtime_status.ip`.

---

## Data Streams (iOS → Server)

Remote clients can push typed data over the existing WebSocket connection:

```json
{"type": "data", "stream": "location", "payload": {"lat": 45.1, "lng": 9.2, "accuracy": 10.0}}
```

Handled in `src/frontend/api/ws.rs` → `handle_data_msg()`. Dispatched to:

| Stream | Handler | Notes |
|---|---|---|
| `location` | `state.location_manager.update("remote", ...)` | Stores in-memory; existing `latest()` / `all()` queries work |
| other | `warn!` log | Reserved for future streams (health, etc.) |

---

## Limitations

### `tailscale_sys` (system daemon)

- **Requires `tailscaled` on the host**: if the daemon is not running or the device is not logged in, `start()` fails immediately with a clear error.
- **IP can change**: if the device rejoins the tailnet with a different IP, the plugin must be restarted to pick up the new address. The server binds to a specific IP, not `0.0.0.0`.

### `tailscale` (embedded, tailscale-rs v0.3)

- **DERP-relay only**: direct WireGuard holepunching is not yet implemented (issue #151). Latency is slightly higher than native WireGuard.
- **Known reconnect bugs**: after a node restart, existing connections can hang for ~15 s (issue #11). DERP connectivity can be lost after a control plane reconnect (issue #26).
- **Unaudited cryptography**: do not use for highly sensitive data until an official audit is complete.
- **Breaking changes**: the library is pre-1.0. The `TS_RS_EXPERIMENT=this_is_unstable_software` env var is required at runtime (set by `run.sh`).
- **Auth key expiry**: auth keys expire. Regenerate and call `configure_plugin` with the new value if the plugin fails to connect.

---

## Feature Flag

`tailscale-rs` is compiled only when the `remote-tailscale` feature is active.
It is **off by default**: the crate pulls the pure-Rust `tailscale` library, which
internally forces the `aws-lc-rs` crypto backend (a cmake/NASM C build) back into
the whole workspace, defeating the ring-only static-binary story
(see [build-and-distribution.md](../build-and-distribution.md)). The recommended
`tailscale_sys` provider needs none of this and is always compiled.

```toml
# crates/plugin-tailscale-remote/Cargo.toml
[features]
default = []                       # embedded provider OFF by default
remote-tailscale = ["dep:tailscale"]

# root Cargo.toml — opt in from the top-level build:
[features]
embedded-tailscale = ["plugin-tailscale-remote/remote-tailscale"]
```

To build **with** the embedded provider (accepts the aws-lc-rs C build):

```sh
cargo build --features embedded-tailscale
```

The default build already omits it — no `--no-default-features` needed.
