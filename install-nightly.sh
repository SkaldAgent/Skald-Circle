#!/usr/bin/env sh
# install-nightly.sh — install the latest nightly build of Skald Circle
#
# Usage:
#   curl -fsSL https://builds.skaldagent.net/install-nightly.sh | bash
#
# Supports Linux (systemd) and macOS ARM64 (launchd).
# Default install dir: ~/.local/share/skald-circle (override with SKALD_DIR).
#
# Inspired by: https://hermes-agent.nousresearch.com/install.sh

set -eu

# ── User overrides ────────────────────────────────────────────────────────────
INSTALL_DIR="${SKALD_DIR:-$HOME/.local/share/skald-circle}"

# ── Colours (if terminal) ─────────────────────────────────────────────────────
if [ -t 1 ]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BOLD=''; NC=''
fi

info()  { printf "${GREEN}%s${NC}\n" "$*"; }
warn()  { printf "${YELLOW}⚠ %s${NC}\n" "$*"; }
err()   { printf "${RED}✖ %s${NC}\n" "$*"; }
header(){ printf "\n${BOLD}%s${NC}\n" "$*"; }

# ── Platform detection ────────────────────────────────────────────────────────
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)  OS="linux"  ;;
    Darwin) OS="darwin" ;;
    *)      err "Unsupported OS: $OS"; exit 1 ;;
esac

case "$ARCH" in
    x86_64)
        ARCH="amd64"
        if [ "$OS" = "darwin" ]; then
            err "Intel Macs are not supported. Apple Silicon (M1+) only."
            exit 1
        fi
        ;;
    aarch64|arm64)
        ARCH="arm64"
        ;;
    *)      err "Unsupported architecture: $ARCH"; exit 1 ;;
esac

# ── Dependency checks ─────────────────────────────────────────────────────────
command -v curl >/dev/null 2>&1 || { err "curl is required but not installed."; exit 1; }

if [ "$OS" = "linux" ]; then
    command -v systemctl >/dev/null 2>&1 || { warn "systemd not found — service will not be installed automatically."; NOSYSTEMD=1; }
elif [ "$OS" = "darwin" ]; then
    command -v launchctl >/dev/null 2>&1 || { err "launchctl not found."; exit 1; }
fi

# ── Download & extract ────────────────────────────────────────────────────────
BASE_URL="https://builds.skaldagent.net"
TARBALL_URL="${BASE_URL}/nightly/skald-circle-nightly-${OS}-${ARCH}.tar.gz"

header "📦 Skald Circle — Nightly Installer"
echo ""
echo "  Platform     : ${OS}/${ARCH}"
echo "  Install dir  : ${INSTALL_DIR}"
echo "  Download     : ${TARBALL_URL}"
echo ""

info "↓ Downloading Skald Circle nightly …"
mkdir -p "$INSTALL_DIR"
curl -fsSL "$TARBALL_URL" | tar xz -C "$INSTALL_DIR" --strip-components=1

if [ ! -x "$INSTALL_DIR/bin/skald" ]; then
    err "Download or extraction failed — skald binary not found."
    exit 1
fi

info "✔ Extracted to ${INSTALL_DIR}"

# ── Python venv (best-effort) ─────────────────────────────────────────────────
info "🔧 Setting up Python virtual environment …"
"$INSTALL_DIR/run.sh" >/dev/null 2>&1 || true

# ── First-run setup (interactive) ─────────────────────────────────────────────
if [ -t 0 ] && [ -x "$INSTALL_DIR/bin/skald-setup" ]; then
    header "⚙️  First-time setup"
    echo "  You will be asked to configure your LLM provider and create an admin user."
    echo ""
    "$INSTALL_DIR/bin/skald-setup"
    echo ""
fi

# ── Install daemon ────────────────────────────────────────────────────────────
if [ "$OS" = "linux" ] && [ -z "${NOSYSTEMD:-}" ]; then
    header "⚡ Installing systemd user service …"

    mkdir -p "$HOME/.config/systemd/user"

    cat > "$HOME/.config/systemd/user/skald-circle.service" <<- SERVICE
[Unit]
Description=Skald Circle (nightly)
Documentation=https://skaldagent.net
After=network.target

[Service]
Type=simple
ExecStart=${INSTALL_DIR}/run.sh
WorkingDirectory=${INSTALL_DIR}
Restart=on-failure
RestartSec=5
Environment=SKALD_BIN=${INSTALL_DIR}/bin/skald
Environment=SKALD_SETUP_BIN=${INSTALL_DIR}/bin/skald-setup

[Install]
WantedBy=default.target
SERVICE

    systemctl --user daemon-reload
    systemctl --user enable --now skald-circle.service

    info "✔ Service installed and started"
    echo ""
    echo "  Status:  systemctl --user status skald-circle"
    echo "  Logs:    journalctl --user -u skald-circle -f"

elif [ "$OS" = "darwin" ]; then
    header "⚡ Installing launchd agent …"

    mkdir -p "$HOME/Library/LaunchAgents" "$INSTALL_DIR/logs"

    PLIST="$HOME/Library/LaunchAgents/com.skald.circle.plist"

    cat > "$PLIST" <<- PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.skald.circle</string>

    <key>ProgramArguments</key>
    <array>
        <string>${INSTALL_DIR}/run.sh</string>
    </array>

    <key>WorkingDirectory</key>
    <string>${INSTALL_DIR}</string>

    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>

    <key>StandardOutPath</key>
    <string>${INSTALL_DIR}/logs/stdout.log</string>
    <key>StandardErrorPath</key>
    <string>${INSTALL_DIR}/logs/stderr.log</string>

    <key>EnvironmentVariables</key>
    <dict>
        <key>SKALD_BIN</key>
        <string>${INSTALL_DIR}/bin/skald</string>
        <key>SKALD_SETUP_BIN</key>
        <string>${INSTALL_DIR}/bin/skald-setup</string>
    </dict>
</dict>
</plist>
PLIST

    launchctl load "$PLIST"

    info "✔ Agent installed and started"
    echo ""
    echo "  Status:  launchctl list com.skald.circle"
    echo "  Logs:    tail -f ${INSTALL_DIR}/logs/stdout.log"

elif [ -n "${NOSYSTEMD:-}" ]; then
    warn "systemd not available — start manually: ${INSTALL_DIR}/run.sh"
fi

echo ""
info "✅ Skald Circle (nightly) installed successfully!"
echo ""
echo "  ${INSTALL_DIR}/run.sh"
echo "  ${INSTALL_DIR}/bin/skald"
echo "  ${INSTALL_DIR}/bin/skald-setup"
