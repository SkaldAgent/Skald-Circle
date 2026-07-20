#!/usr/bin/env sh
# uninstall.sh — remove Skald Circle and all its data
#
# Usage:
#   ./uninstall.sh
#
# Stops the daemon (systemd on Linux, launchd on macOS), removes the
# service/agent file, then deletes the entire installation directory
# (config, database, everything).
#
# Set SKALD_DIR before running if you installed to a custom location:
#   SKALD_DIR=/opt/skald-circle ./uninstall.sh

set -eu

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

# ── Determine install directory ───────────────────────────────────────────────
# Default: the directory this script lives in (i.e. the bundle root).
if [ -n "${SKALD_DIR:-}" ]; then
    INSTALL_DIR="$SKALD_DIR"
else
    INSTALL_DIR="$(cd "$(dirname "$0")" && pwd)"
fi

# ── Detect OS ─────────────────────────────────────────────────────────────────
OS="$(uname -s)"

echo ""
printf "\033[1m🗑️  Skald Circle — Uninstaller\033[0m\n"
echo ""
echo "  This will permanently delete Skald Circle and all its data:"
echo "    ${INSTALL_DIR}"
echo ""

if [ -t 0 ]; then
    printf "%s " "Are you sure? Type 'yes' to continue: "
    read -r CONFIRM
    [ "$CONFIRM" = "yes" ] || { echo "Aborted."; exit 0; }
    echo ""
fi

# ── Stop & remove daemon ──────────────────────────────────────────────────────
case "$OS" in
    Linux)
        SERVICE_NAME="skald-circle.service"
        SERVICE_PATH="$HOME/.config/systemd/user/$SERVICE_NAME"

        if command -v systemctl >/dev/null 2>&1; then
            if systemctl --user is-enabled "$SERVICE_NAME" >/dev/null 2>&1; then
                info "⏹️  Stopping Skald Circle service …"
                systemctl --user stop "$SERVICE_NAME" 2>/dev/null || true
                systemctl --user disable "$SERVICE_NAME" 2>/dev/null || true
            fi

            if [ -f "$SERVICE_PATH" ]; then
                info "🗑️  Removing systemd service file …"
                rm -f "$SERVICE_PATH"
                systemctl --user daemon-reload 2>/dev/null || true
            fi
        else
            warn "systemd not found — skipping service removal."
        fi
        ;;

    Darwin)
        PLIST="$HOME/Library/LaunchAgents/com.skald.circle.plist"

        if [ -f "$PLIST" ]; then
            info "⏹️  Stopping Skald Circle agent …"
            launchctl unload "$PLIST" 2>/dev/null || true
            info "🗑️  Removing launchd plist …"
            rm -f "$PLIST"
        else
            warn "launchd plist not found at ${PLIST}"
        fi
        ;;

    *)
        warn "Unknown OS: $OS — skipping daemon removal."
        ;;
esac

# ── Remove installation directory ─────────────────────────────────────────────
if [ -d "$INSTALL_DIR" ]; then
    info "🗑️  Removing installation directory …"
    rm -rf "$INSTALL_DIR"
    info "✔ Removed ${INSTALL_DIR}"
else
    warn "Installation directory not found: ${INSTALL_DIR}"
fi

echo ""
info "✅ Skald Circle has been uninstalled."
echo "  If you want to reinstall:"
echo "    curl -fsSL https://builds.skaldagent.net/install.sh | bash"
echo "    curl -fsSL https://builds.skaldagent.net/install-nightly.sh | bash"
echo ""
