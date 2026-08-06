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

**Follow-up**: `After=docker.service` was dropped too. It never did anything — a _user_ manager has no view of system units, so the ordering was silently ignored rather than merely advisory, and keeping it suggested a guarantee that was not there. What actually handles the boot race is `Restart` (see below): the server fails fast when the Docker daemon is unreachable, and systemd brings it back a few seconds later.

## Bug fix: the server dies when you log out ✅

**Problem**: `systemctl --user start skald-circle` worked, but closing the SSH session killed the server — and it never came up at boot. Not an application bug: a `--user` unit runs under the per-user manager (`user@UID.service`), which systemd starts at first login and **stops when the user's last session ends**, tearing down every user service in the cgroup. No crash, no error in the journal — the whole cgroup is simply killed.

**Fix**: both installers now run `loginctl enable-linger $USER` after installing the unit (helper `enable_linger`, tried unprivileged first, then `sudo -n`, then interactive `sudo`, and only warns if all three fail — a missing linger must never abort an install). `update.sh` carries the same helper so an installation predating this fix is healed by an ordinary update.

**Also**: `Restart=on-failure` → `Restart=always`. `run.sh` exits 0 on _any_ graceful shutdown, including one nobody asked for (a stray SIGTERM to the server), which `on-failure` reads as a clean stop and leaves the box down. An explicit `systemctl --user stop` is unaffected — systemd never restarts after a requested stop. With lingering on, this is also what absorbs the boot race against Docker.

## Bug fix: update.sh never stopped or restarted the service ✅

**Problem**: `stop_service` and `start_service` matched `case "$OS" in Linux) … Darwin)`, but `$OS` had already been normalized to `linux`/`darwin` at the top of the script. Every branch fell through: both functions were no-ops. So the updater extracted the tarball **over the running binary** (`ETXTBSY` on Linux, aborting the update mid-way) and, when extraction did succeed, left the old build running in memory with the safety-net trap firing a restart that was itself a no-op. The careful stop → wait-for-exit → extract ordering the file documents at the top had not been executing at all.

**Fix**: matched the normalized lowercase values, with a comment at the seam saying why the capitalization is load-bearing. `uninstall.sh` was correct on its own (it matched raw `uname -s`), but it was the odd one out of four sibling scripts — which is how a `case` gets copied into the wrong one — so it now normalizes like the others.

## Bug fix: the installers piped curl straight into tar ✅

**Problem**: `curl -fsSL "$TARBALL_URL" | tar xz -C "$INSTALL_DIR"`. A truncated download half-extracts, and the installer explicitly supports reinstalling over an existing install — so an interrupted download left a tree mixing old and new files, with no error saying so. `update.sh` had guarded against exactly this since it was written; the installers had not.

**Fix**: download to a temp file, verify it extracts and carries `bin/skald` in a staging dir, and only then write to the install directory. Same ordering, same reasoning as `update.sh`.

## Improvement: update.sh now drops files deleted upstream ✅

**Problem**: extracting over the install directory only ever adds and overwrites. Anything removed upstream survived every future update — a renamed page under `docs/` kept being mounted read-only into every container for the assistant to read, a deleted command kept being discovered.

**Fix**: after extracting, prune from the directories the tarball owns end to end (`web/`, `commands/`, `skills/`, `docs/`) whatever the already-verified staging copy does not have, then remove the directories left empty. Pruning _after_ the extraction rather than replacing the directory keeps every intermediate state a complete install, and the only files removed are ones the new build has verifiably dropped.

`agents/` is deliberately excluded: adding an agent is a documented extension point (`agents/<id>/meta.json` + `AGENT.md`), so the directory is not ours alone and pruning it would delete somebody's work — at the price of an upstream-deleted agent lingering. `bin/` is excluded too: two files, both overwritten every time.

## Bug fix: uninstall.sh could remove containers that are not ours ✅

**Problem**: `docker ps -aq --filter 'name=skald-'` feeding `docker rm -f`. Docker's name filter is a regex matched _anywhere_ in the name, not a prefix, so any unrelated container whose name merely contains `skald-` was force-removed.

**Fix**: anchored to `name=^skald-`. Ours are always `skald-{userid}`.

**Also**: the uninstaller now reports that systemd lingering is still enabled and how to turn it off, rather than disabling it. It is a persistent per-user setting that other `systemctl --user` services may be relying on by now, so taking it back silently would stop those too — the note leaves the choice to the human.

## Not done: update.sh does not refresh the systemd unit

The unit is generated in one place (the installers) and `update.sh` deliberately does not rewrite it — clobbering a hand-edited unit as a side effect of an update is the kind of surprise worth avoiding, and duplicating the template into a second script is how the two drift. Consequence: unit changes (such as `Restart=always`) reach an existing box only by re-running the installer, which is idempotent — `skald-setup` is a no-op once an admin exists.

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

- `scripts/` removed — CI scripts live in `ci/` (tracked by git); the legacy MCP servers it held are superseded by marketplace connectors
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
