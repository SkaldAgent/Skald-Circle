# Skald Circle — SKALD

## Current status

New application with agents and chatbots to help families and small groups collaborate, with supervised chat for children and vulnerable people.

### Agent icons — completed ✅

All 11 agents now have **Vector Paintings** icons (painterly vector, warm and family-friendly), generated via ComfyUI:

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
| TIC | 👁️ Cat | ✅ |
| Business Analyst | 💼 Magpie | ✅ |

### Refactoring — completed ✅

- Removed Tauri/desktop dependency (`tauri.conf.json`, `src/desktop/`, `icons/`, `docs/desktop.md`, gen schemas/)
- Removed `build.rs` (no longer needed)
- New i18n system (core-api + plugin-mobile-connector + web)
- Configuration system refactoring

### Auto-build CI/CD ✅

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

### Next steps

- Create `release` branch on Gitea with branch protection (PR via UI)
- Test release workflow with a PR
- Build first macOS ARM64 binary on MacBook, upload to `builds.skaldagent.net`

### macOS support

**Supported**: macOS ARM64 (Apple Silicon M1+), Intel not supported.

| Aspect | Status | Notes |
|--------|--------|-------|
| **Install script** (`install.sh`) | ✅ | Auto-detects macOS, uses launchd |
| **Nightly install** (`install-nightly.sh`) | ✅ | Same logic |
| **Uninstall script** (`uninstall.sh`) | ✅ | Handles launchctl |
| **Package script** (`ci/package.sh`) | ✅ | Accepts `--os darwin`, strips best-effort |
| **Binary** | ⏳ Not yet built | Build natively on MacBook, deploy to builds.skaldagent.net |

#### How to build for macOS (on MacBook)

```sh
cargo build --release -p skald-setup -p skald  # includes whisper
./ci/package.sh --version v0.1.0 --os darwin --arch arm64 \
  --target-dir target/release --output dist/
```

Upload the resulting `dist/skald-circle-v0.1.0-darwin-arm64.tar.gz` to the NiPoGi's `builds.skaldagent.net/releases/v0.1.0/` directory.

#### Cross-compilation from NiPoGi (research notes 🧪)

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
