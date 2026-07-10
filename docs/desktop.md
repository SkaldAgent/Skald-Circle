# Desktop bundle (Tauri)

Skald ships in **two shapes** from the same source tree, selected by the
`desktop` cargo feature:

| Shape | How it runs | How the user reaches it | Typical host |
| --- | --- | --- | --- |
| **Headless server** (default, no `desktop` feature) | The binary blocks on its own tokio runtime, as before. | Browser on `http://<host>:<port>` (LAN-friendly, binds `0.0.0.0`). | Linux server, dev workstation, Docker container. |
| **Desktop bundle** (`--features desktop`) | A Tauri event loop wraps the same headless backend; the backend runs as a task on Tauri's shared tokio runtime. A system-tray icon exposes `Open` / `Quit`; the webview loads `http://127.0.0.1:<port>`. | Double-click the `.app` / `.exe` / `.AppImage`. | End-user macOS / Windows / Linux machine. |

The `desktop` feature is **opt-in and default-off**: every existing build path
(`cargo build`, `run.sh`, `Dockerfile`) is unchanged because `--no-default-features`
already excludes it.

---

## Build

### Dev mode (debug, no bundle)

```sh
cargo run --features desktop
```

A real Tauri window + tray icon appears. The webview loads
`http://127.0.0.1:<config-port>` from the running dev binary; the binary reads
`config.yml`, `agents/`, `skills/`, `web/` from the crate root exactly as the
headless build does (the cwd is **not** relocated in dev mode — see
*Data directory* below).

### Distributable bundle (release, packaged)

```sh
cargo tauri build --features desktop
```

This requires the `tauri-cli`:

```sh
cargo install tauri-cli --version "^2"
```

Produces native installers under `target/release/bundle/`:

| OS | Artifact |
| --- | --- |
| macOS | `Skald.app`, `Skald.dmg` |
| Windows | `Skald.msi`, `Skald.exe` (NSIS) |
| Linux | `Skald.deb`, `Skald.AppImage` |

> **macOS deployment target.** The default `whisper-local` feature compiles
> `whisper.cpp` (C++), whose `ggml-backend-reg.cpp` uses
> `std::filesystem::path` — introduced in macOS 10.15. `cargo tauri build`
> injects `MACOSX_DEPLOYMENT_TARGET` from
> `tauri.conf.json > bundle.macOS.minimumSystemVersion` (Tauri's default is
> `"10.13"`), on which `std::filesystem::path` is marked *unavailable*, so the
> ggml build fails with ~20 `'path' is unavailable` errors.
>
> The fix has **three coordinated pieces**, all pinned to `"10.15"` (the exact
> floor that unblocks `std::filesystem`):
>
> 1. **`.cargo/config.toml`** sets **two** forced env vars. Both are needed
>    because the C++ compile otherwise ends up with two conflicting
>    `-mmacosx-version-min` flags and clang lets the *last* one win:
>    - `MACOSX_DEPLOYMENT_TARGET` → the `cc` crate turns this into the CFLAGS'
>      `-mmacosx-version-min`.
>    - `CMAKE_OSX_DEPLOYMENT_TARGET` → `whisper-rs-sys`'s `build.rs` forwards any
>      `CMAKE_*` env var to cmake as `-DCMAKE_OSX_DEPLOYMENT_TARGET=…`, which
>      sets CMake's *own* `-mmacosx-version-min` **and overrides a value cached
>      in a stale `CMakeCache.txt`**. Without this, CMake's cached 10.13 wins.
>
>    `force = true` makes cargo override whatever the Tauri CLI injects
>    (`MACOSX_DEPLOYMENT_TARGET=10.13`), so the C/C++ build is
>    **deterministically** 10.15 regardless of Tauri's env-propagation. It also
>    gives the headless binary a 10.15 portability floor.
> 2. **`tauri.conf.json > bundle.macOS.minimumSystemVersion = "10.15"`** — the
>    value baked into the bundle's `Info.plist` as `LSMinimumSystemVersion`.
> 3. If a build still fails after a target change, **wipe the stale CMake
>    caches**: `rm -rf target/*/build/whisper-rs-sys-*` (`cargo clean -p
>    whisper-rs-sys` does not always clear the cmake `out/` dir). Piece 1's
>    explicit `-D` flag makes this rarely necessary, but it is the escape hatch.
>
> Keep pieces 1 and 2 in sync. Raise all to `"11.0"` / `"12.0"` only if you want
> an Apple-Silicon-only bundle.

For cross-platform CI builds use a GitHub Actions matrix (the
[`tauri-apps/tauri-action`](https://github.com/tauri-apps/tauri-action) is the
canonical setup).

---

## Architecture

```
                ┌────────────────── main.rs ──────────────────┐
                │ install rustls ring provider                 │
                │ init_logging()                               │
                │                                              │
                │  ┌── cfg(feature = "desktop") ──┐            │
                │  │ desktop::run()                │            │
                │  │   tauri::Builder              │            │
                │  │     .setup(spawn backend)     │            │
                │  │     .on_window_event(hide)    │            │
                │  │     .run(ExitRequested→       │            │
                │  │           shutdown_backend)   │            │
                │  └───────────────────────────────┘            │
                │  ┌── cfg(not(feature = "desktop")) ─┐        │
                │  │ tokio runtime → async_main()      │        │
                │  │   run_backend()                   │        │
                │  │   wait_for_shutdown_signal()      │        │
                │  │   shutdown_backend()              │        │
                │  └───────────────────────────────────┘        │
                └──────────────────────────────────────────────┘
```

### Module map

| Path | Role |
| --- | --- |
| `src/main.rs` | Dispatcher. Installs the rustls provider, sets up tracing, then branches on the `desktop` feature. Exposes `run_backend()` and `shutdown_backend()` shared by both entry points. |
| `src/desktop/mod.rs` | Tauri shell. Builds the system tray + menu, creates the main `WebviewWindow` (URL derived from `config.yml`'s `server.port`), spawns the backend on Tauri's shared tokio runtime, and handles graceful shutdown. Compiled only under `--features desktop`. |
| `src/config.rs` | `bootstrap_data_dir()` relocates the cwd to a per-user data dir when running inside a `.app` bundle (no-op in dev mode and headless mode). |
| `src/core/tools/restart.rs` | Branches on the feature: under `desktop`, restarts the Tauri process (cleanup + respawn the same binary, **no rebuild** — the bundle is read-only); otherwise `libc::_exit(-1)` so `run.sh` rebuilds. |
| `build.rs` | Calls `tauri_build::build()` under the `desktop` feature; no-op otherwise. |
| `tauri.conf.json` | Tauri bundle config: identifier, icons, security. The main window is **not** declared here — it is built programmatically in the setup hook so the URL can read the backend port from `config.yml`. |
| `capabilities/default.json` | Tauri v2 capability set for the `main` window (window show/hide/focus permissions). The frontend never invokes Tauri's JS API — it is the regular Skald web app served by Axum. |
| `icons/` | App icon (`icon.png`, `32x32.png`, `128x128.png`, `128x128@2x.png`, `icon.icns`, `icon.ico`) and a monochrome tray template (`tray-template.png`). See *Icons* below. |

### Why a single binary, not a launcher

`skald` is a binary crate (`src/main.rs`, no `src/lib.rs`). Putting the Tauri
shell in `src/desktop/` behind `#[cfg(feature = "desktop")]` — instead of a
separate crate — keeps everything private (no `pub` leakage) and lets
`tauri::generate_context!()` find `tauri.conf.json` in `CARGO_MANIFEST_DIR`.

### Why Tauri's runtime, not a fresh tokio

`tauri::async_runtime::spawn` lands the task on Tauri's internal tokio
runtime. One runtime, one process, no IPC bridge — the webview talks to the
backend over plain HTTP/WS on `127.0.0.1`, exactly like a browser would.

---

## System tray + window policy

* A single tray icon is built in `build_tray()` with two menu items: **Open**
  and **Quit**.
* **Left-click** the tray icon toggles the main window (show+focus or hide).
* **Open** menu item shows + focuses the main window.
* The window's close button (traffic-light red on macOS, X on Windows/Linux)
  is intercepted in `on_window_event` → `WindowEvent::CloseRequested` and
  turned into a `hide()`. The app keeps running in the tray.
* **Quit** (and Cmd+Q / system shutdown) triggers `RunEvent::ExitRequested`.
  The handler spawns `shutdown_backend()` on the async runtime, waits for it
  to drain the HTTP server / Skald managers / DB pool, then calls
  `app.exit(0)`. An `AtomicBool` re-entrancy guard prevents the second
  `ExitRequested` (emitted by the `app.exit(0)` itself) from looping.

---

## Data directory

| Mode | Working directory at startup |
| --- | --- |
| Headless (`cargo run`, `run.sh`, Docker) | Whatever the user launched from (today, the crate root). Unchanged. |
| Desktop dev (`cargo run --features desktop`) | The crate root — same as headless, so `config.yml`, `agents/`, `skills/`, `web/` resolve from the source tree. |
| Desktop bundle (inside `Skald.app/Contents/MacOS/`) | Relocated to the OS per-user data dir, see table below. |

When packaged as a `.app` (or Windows / Linux equivalents), the process cwd is
typically `/`. `bootstrap_data_dir()` detects this (via
`running_from_bundle()` — a `.app/` heuristic on macOS, currently always-false
on Windows/Linux until those targets land) and `set_current_dir`s to the
per-user data dir:

| OS | Location |
| --- | --- |
| macOS | `~/Library/Application Support/Skald` |
| Windows | `%APPDATA%\Skald` (= `C:\Users\<u>\AppData\Roaming\Skald`) |
| Linux | `~/.local/share/Skald` |

This makes every relative path in `config.yml` (`db.path`, `web.static_dir`,
`data/`, `secrets/`, `models/`, …) resolve under the user's data dir without
touching the source tree.

If `config.yml` is missing on first launch, it is seeded from
`DEFAULT_CONFIG_EMBEDDED` (the `default.config.yaml` baked into the binary via
`include_str!`).

**Logs.** `init_logging()` runs *before* the cwd is relocated, so a relative
`logs/` would land against the launch cwd (`/` for a Finder-launched `.app`) and
silently fail to write. `config::resolved_log_dir()` returns an **absolute**
path under the data dir (`~/Library/Application Support/Skald/logs`) in a bundle,
so logs always land in a writable place; headless and desktop-dev keep the
relative `logs/`.

### Read-only bundled assets

The relocated data dir holds only **mutable** state (db, config, logs, secrets,
models, uploads). The **read-only** assets the backend needs — `agents/`,
`web/`, `skills/`, `commands/` — are packaged into the bundle's `Resources/`
dir via `tauri.conf.json > bundle > resources`, and made reachable from the
relocated cwd by `link_bundled_assets()` (in `src/config.rs`), which runs right
after the relocation:

* For each asset name it creates a **symlink** in the data dir pointing at the
  copy inside `…/Skald.app/Contents/Resources/<name>`. Symlinking (not copying)
  keeps the assets in lock-step with the installed app version.
* A pre-existing **real** directory in the data dir is treated as a user
  override and left untouched; only stale symlinks are refreshed each launch.

Without this step `Skald::new` fails on launch with *"Failed to read agents
directory 'agents'"* and the app exits immediately — the assets live next to the
binary, not in the freshly-relocated cwd.

> **Note (App Translocation).** An unsigned, quarantined `.app` launched from
> `~/Downloads` or a mounted `.dmg` is run from an ephemeral read-only path by
> Gatekeeper, so the symlink targets change per launch (they are recreated each
> time, so this is harmless). Moving the app to `/Applications` clears the
> quarantine and stabilises the paths.

---

## `restart` tool behaviour

`src/core/tools/restart.rs` branches on the feature flag:

| Mode | Behaviour |
| --- | --- |
| Headless | `libc::_exit(-1)` (= exit code 255). `run.sh` sees 255, runs `cargo build`, relaunches. **Rebuilds** the binary — used after the agent edits source code. |
| Desktop | `AppHandle` is fetched from `desktop::app_handle()` (a `OnceLock` populated in the setup hook); then `handle.cleanup_before_exit()` + `std::process::Command::new(current_exe).spawn()` + `std::process::exit(0)`. **No rebuild** — the bundled binary is read-only. Useful for picking up `config.yml` / DB changes that are only read at startup. |

See also [self-rewriting.md](self-rewriting.md).

---

## Icons

Two families live under `icons/`:

| Family | Files | Purpose |
| --- | --- | --- |
| App icon (colour) | `icon.png` (1024×1024 source), `32x32.png`, `128x128.png`, `128x128@2x.png`, `icon.icns` (macOS), `icon.ico` (Windows) | Window icon, bundle icon. |
| Tray template (monochrome) | `tray-template.png` (32×32, black on transparent) | macOS menu-bar template image (auto-recoloured by the system for dark/light). On Windows/Linux a coloured icon would be more visible — currently `default_window_icon()` is used as a fallback everywhere until a Tauri 2.x image-loading API is wired in. |

The current subject is a stylised amber feather on a dark rounded square
(Skald = Norse *bard*). To regenerate from a new design, edit
`/tmp/gen_all_icons.py` (Pillow) and re-run `iconutil` for `.icns` and `magick`
for `.ico`. Sources live with the icons; the script is not yet committed.

---

## Known limitations / next steps

* **`LSUIElement` (menu-bar-only on macOS).** Removed from `tauri.conf.json`
  because Tauri v2 dropped the `bundle.macOS.infoPlist` key (v1) — the v2 way
  is a custom `Info.plist` file under `src-tauri/`. Until the crate layout
  grows a `src-tauri/` directory, the app shows both a Dock icon and a tray
  icon. Add the `Info.plist` to make it menu-bar-only.
* **Tray template icon.** The bundled `tray-template.png` is not yet wired up
  (Tauri 2.11.5's `Image::from_path` / `from_bytes` API surface differs from
  later versions). The tray currently reuses `app.default_window_icon()`, which
  is the colour app icon — visible on macOS but not template-styled.
* **Bundled assets — done.** `agents/`, `web/`, `skills/`, `commands/` are
  packaged via `bundle.resources` and symlinked into the data dir by
  `link_bundled_assets()` (see *Read-only bundled assets* above). Remaining
  polish: user-agent overrides beyond the symlink escape-hatch, and refreshing
  the copy-on-write story for signed/notarized distribution.
* **Windows/Linux bundle.** `running_from_bundle()` currently only detects
  `.app/` on macOS; Windows (`Program Files`) and Linux
  (`/usr/share`/`/opt`) detection is TBD.
* **CI matrix.** No GitHub Actions workflow yet for cross-platform bundle
  builds.

---

## When to update this file

* The `desktop` feature or its dependencies change.
* The tray menu, window policy, or shutdown path change.
* The data-directory relocation heuristic changes (new platform, new path).
* The `restart` tool's desktop-mode behaviour changes.
* The packaging story is completed (`bundle.resources`, `Info.plist`, CI).
