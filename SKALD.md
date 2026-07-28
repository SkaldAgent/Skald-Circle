# Skald Circle — SKALD

_This file MUST be written in English. All project notes, decisions, and documentation here are in English._


## Installation

### Stable release

```sh
curl -fsSL https://builds.skaldagent.net/install.sh | bash
```

### Nightly (latest automatic build)

```sh
curl -fsSL https://builds.skaldagent.net/install-nightly.sh | bash
```

### Requirements

| Required | Notes |
|----------|-------|
| **Docker** | User container sandbox. The installer can install it |
| **Linux (amd64/arm64)** or **macOS ARM64 (Apple Silicon)** | Intel Mac not supported |
| **systemd** (Linux) or **launchd** (macOS) | For running as a service |
| **Python 3** (optional) | For Python MCP servers (Gmail, GCal, GMaps, weather, SSH) and local TTS plugins |
| **Node.js ≥ 18** (optional) | For WhatsApp MCP server |

The installer checks each requirement and offers to install Docker if missing.
Python and Node.js are optional — the server starts regardless, but certain MCP servers won't work.

### What it does

1. Downloads the tarball from `builds.skaldagent.net`
2. Extracts to `~/.local/share/skald-circle/` (or `$SKALD_DIR`)
3. Configures the service (systemd user service / launchd agent)
4. Runs `skald-setup` to create the admin account
5. The server starts at `https://localhost:8443`

### Uninstallation

```sh
curl -fsSL https://builds.skaldagent.net/install.sh | bash
# The tarball contains uninstall.sh:
~/.local/share/skald-circle/uninstall.sh
```

Or, after installation: `~/.local/share/skald-circle/uninstall.sh`

## Bug fix: uninstall.sh fails on Docker-owned files in homes/ ✅

**Problem**: `uninstall.sh` runs `rm -rf "$INSTALL_DIR"` as the normal user, but `homes/` contains files created by Docker containers running under different UIDs (often root). The removal fails with "Permission denied" on those files, leaving a broken install behind.

**Fix**: if `rm -rf` fails (non-zero exit), the script retries with `sudo rm -rf`. If even sudo fails, it prints an error message and exits non-zero so the user knows manual cleanup is needed.

### From source

```sh
git clone https://github.com/.../skald-circle.git
cd skald-circle
cargo build --release
./run.sh
```

## Current status

New application with agents and chatbots to help families and small groups collaborate, with supervised chat for children and vulnerable people.

## Installer & startup architecture

```
install.sh
  ├── extracts tarball
  ├── creates .venv (inline — does NOT call run.sh)
  └── runs skald-setup for interactive config

systemd service → ExecStart=run.sh
  run.sh (supervisor)
    ├── creates .venv if it doesn't exist (for local dev)
    ├── runs skald-setup (first run only)
    └── loop: executes skald binary, restart on exit 255
```

**Rule**: `install.sh` must NEVER call `run.sh`. The venv is created inline in the installer.
`run.sh` is only for the service supervisor or local development.
`skald-setup` is the only setup executable called by install.sh.

## Bug fix: install.sh stuck on "Setting up Python virtual environment" ✅

**Problem**: the installer called `"$INSTALL_DIR/run.sh"` to create the venv. But `run.sh` after the venv runs `skald-setup` and then the server in a loop, hanging the installer forever.

**Fix**: the venv is now created *inline* in `install.sh` and `install-nightly.sh`, using the same logic as `run.sh` (uv > python3) but without starting the server.

## Bug fix: "Unit docker.service not found" in user service ✅

**Problem**: the systemd user unit had `Requires=docker.service`, but `docker.service` is a system-level unit (not a user unit). `systemctl --user` couldn't find it and refused to start Skald.

**Fix**: removed `Requires=docker.service` from the user unit template in both install scripts. Kept `After=docker.service` (advisory, doesn't block if the unit isn't found).

## Bug fix: skald-setup non interattivo con curl | bash ✅

**Problem**: `skald-setup` controlla `isatty(0)`, ma con `curl ... | bash` stdin è un pipe, quindi saltava senza chiedere username/password. L'installer arrivava fino in fondo ma senza aver creato l'admin.

**Fix**: se `IS_INTERACTIVE=false` ma `/dev/tty` esiste, l'installer chiama `skald-setup </dev/tty`.


### Agent icons — completed ✅

All agents now have **Vector Paintings** icons (painterly vector, warm and family-friendly), generated via ComfyUI:

**Chat agents — warm animals:**

| Agent | Animal | Status |
|-------|--------|--------|
| Main Assistant | 🦊 Fox | ✅ |
| Project Coordinator | 🦡 Badger | ✅ |
| Researcher | 🐿️ Squirrel | ✅ |
| Generalist | 🦫 Beaver | ✅ |
| Code Explorer | 🕵️ Meerkat | ✅ |
| Software Architect | 🏗️ Heron | ✅ |
| Software Engineer | 🔧 Bear | ✅ |
| Spec Writer | 📝 Owl | ✅ |
| Tech Lead | 👑 Deer | ✅ |
| Business Analyst | 💼 Magpie | ✅ |
| Companion | 🦦 Otter | ✅ |

**System agents — insect family:**

| Agent | Animal | Status |
|-------|--------|--------|
| Event triage | 🕷️ Spider | ✅ |
| Private Memory Lint | ✨ Firefly | ✅ |
| Shared Memory Lint | 🐝 Bee | ✅ |
### Refactoring — completed ✅

- Removed Tauri/desktop dependency (`tauri.conf.json`, `src/desktop/`, `icons/`, `docs/desktop.md`, gen schemas/)
- Removed `build.rs` (no longer needed)
- New i18n system (core-api + plugin-mobile-connector + web)
- Configuration system refactoring

## Auto-build CI/CD (NiPoGi)

Automatic build on NiPoGi with Gitea Actions (native runner v2.1.0):

| Component | File | Status |
|---|---|---|
| `ci/package.sh` | Creates distribution tarballs from compiled binaries | ✅ |
| `ci/verify-version.sh` | Verifies that a release hasn't been built yet | ✅ |
| `.gitea/workflows/nightly.yml` | Push to `main` → build amd64+arm64 → nightly/ | ✅ |
| `.gitea/workflows/release.yml` | PR check `verify-version` + merge → build → releases/v{ver}/ | ✅ |
| **Native runner** on NiPoGi | v2.1.0, host-mode systemd service, label `linux-amd64` | ✅ |
| **Cross toolchain** (arm64) | `gcc-aarch64-linux-gnu` + `rustup target add` | ✅ |
| **Caddy `builds.skaldagent.net`** | file_server browse (directory listing) | ✅ |
| **Route53 `builds.skaldagent.net`** | A record → 145.40.169.107 | ✅ |
| **CI cache** | Persistent `CARGO_TARGET_DIR` at `/home/dguiducci/.cache/skald-ci/target` | ✅ |
| **`install.sh`** | One-liner script `curl ... | bash` — Linux (systemd) + macOS ARM64 (launchd) | ✅ |
| **`install-nightly.sh`** | One-liner script for nightly builds — same OS support | ✅ |
| **`uninstall.sh`** | Bundled in tarball — stops service/agent, removes everything | ✅ |
| **`releases/LATEST`** | Auto-updated by release workflow to track latest version | ✅ |

### Technical notes

- `scripts/` in `.gitignore` — CI scripts moved to `ci/` (tracked by git)
- Build without `whisper-local` on Linux (`--no-default-features`)
- `aarch64-linux-gnu-strip` for ARM64 binaries
- `actions/checkout@v4` works (native runner has Node.js)
- macOS ARM64 supported via `install.sh` / `install-nightly.sh` (auto-detects OS, uses launchd)


## macOS package script (`ci/package-macos.sh`)

Script to build and deploy the macOS ARM64 package directly from the MacBook.

| Detail | Value |
|--------|-------|
| **File** | `ci/package-macos.sh` |
| **Branch `release`** | Build + version check (curl) + upload to `releases/v{ver}/` + update LATEST |
| **Branch `main`** | Build + upload to `nightly/` (no version check) |
| **Other branches** | ❌ Abort |
| **Remote host** | `skaldserver` (SSH alias → `192.168.1.100`, user `dguiducci`, key `id_ed25519_skaldserver`) |
| **Remote path** | `/var/www/builds.skaldagent.net/` |

### Setup SSH

| Step | Command |
|------|---------|
| Key created | `ssh-keygen -t ed25519 -f ~/.ssh/id_ed25519_skaldserver` |
| `~/.ssh/config` alias | `Host skaldserver` → `HostName 192.168.1.100 User dguiducci IdentityFile ~/.ssh/id_ed25519_skaldserver` |
| Installed on server | `cat ~/.ssh/id_ed25519_skaldserver.pub` → `~/.ssh/authorized_keys` on the NiPoGi |
| MCP SSH registered | `mcp__ssh__add_alias` → alias `skaldserver` (auth: key, sudo: prompt) |

### Operational notes

- Builds with **whisper included** (no `--no-default-features` like on Linux)
- The tarball is uploaded via SCP (`scp` + `ssh` for LATEST)
- `install.sh` / `install-nightly.sh` already support macOS ARM64 (launchd)
- Service homepage at `http://192.168.1.100:8086` — updated with **📦 Builds** card
  → after editing the file, run `docker restart homepage` (bind mount `:ro` doesn't propagate live)


### Next steps

- [x] Script `ci/package-macos.sh` to build and deploy from MacBook (release + nightly)
- Test the script on `main` branch (nightly)
- Test the script on `release` branch (release)
- Create `release` branch on Gitea with branch protection (PR via UI)
- Test release workflow with a PR

## macOS support

**Supported**: macOS ARM64 (Apple Silicon M1+), Intel not supported.

| Aspect | Status | Notes |
|--------|--------|-------|
| **Install script** (`install.sh`) | ✅ | Auto-detects macOS, uses launchd |
| **Nightly install** (`install-nightly.sh`) | ✅ | Same logic |
| **Uninstall script** (`uninstall.sh`) | ✅ | Handles launchctl |
| **Package script** (`ci/package.sh`) | ✅ | Accepts `--os darwin`, strips best-effort |
| **Binary** | ⏳ Not yet built | Build natively on MacBook, deploy to builds.skaldagent.net |

### How to build for macOS (on MacBook)

```sh
cargo build --release -p skald-setup -p skald  # includes whisper
./ci/package.sh --version v0.1.0 --os darwin --arch arm64 \
  --target-dir target/release --output dist/
```

Upload the resulting `dist/skald-circle-v0.1.0-darwin-arm64.tar.gz` to the NiPoGi's `builds.skaldagent.net/releases/v0.1.0/` directory.

### Cross-compilation from NiPoGi (research notes 🧪)

Cross-compiling for `aarch64-apple-darwin` from the NiPoGi using **zig** + **cargo-zigbuild** was attempted but hit blockers.

| Component | Location | Notes |
|-----------|----------|-------|
| **Zig** | `~/.local/bin/zig` (symlink to `/tmp/zig-linux-x86_64-0.14.0/zig`) | v0.14.0, installed manually |
| **macOS SDK** | `/opt/MacOSX/MacOSX11.3.sdk` | From `phracker/MacOSX-SDKs` (GitHub) |
| **Rust targets** | `aarch64-apple-darwin` | via `rustup target add` |
| **`cargo-zigbuild`** | `~/.cargo/bin/cargo-zigbuild` | v0.23.0 |
| **zig wrapper scripts** | `/tmp/zig-wrap-cxx.sh`, `/tmp/zig-ar-wrap.sh` | Handle OpenSSL/Clang flags + SDK paths |

**What works:**
- ✅ Rust std compilation for macOS target
- ✅ OpenSSL compilation from source (via wrapper that remaps `--target=` and provides SDK headers)
- ✅ Rust dependency compilation (tree-sitter, sqlx, tokio, etc.)
- ✅ Single-file C programs compile and link correctly

**What's blocked:**
- ❌ `zig cc` segfaults with `-F` (framework search path) on Linux → can't link against macOS frameworks (CoreFoundation, Security)
- ❌ `zig cc` can't find frameworks without `-F`
- ❌ `libsqlite3-sys` build.rs bug: `is_apple` checks `host.contains("apple") && target.contains("apple")` → forces OpenSSL linkage instead of CommonCrypto on cross-compile (needs upstream fix or `OPENSSL_DIR` workaround)

**The fix would be:**
1. Upstream fix to `libsqlite3-sys` build.rs (`target.contains("apple")` only)
2. Zig fix for `-F` segfault, or use `ld64` instead of zig's linker

**Conclusion**: Cross-compilation is fragile. Build natively on MacBook for now.
