#!/usr/bin/env sh
# update.sh — update Skald Circle to the latest version on the same channel
#
# Usage:
#   ~/.local/share/skald-circle/update.sh
#
# Reads the .release-channel file written by the installer to determine
# whether to pull from the release or nightly channel. On release, checks
# the remote LATEST version first and skips the download if already current.
#
# Stops the service before extracting, then restarts it afterwards.

set -eu

# ── Determine install directory ───────────────────────────────────────────────
if [ -n "${SKALD_DIR:-}" ]; then
    INSTALL_DIR="$SKALD_DIR"
else
    INSTALL_DIR="$(cd "$(dirname "$0")" && pwd)"
fi

CHANNEL_FILE="${INSTALL_DIR}/.release-channel"
if [ ! -f "$CHANNEL_FILE" ]; then
    echo "✖ .release-channel not found in ${INSTALL_DIR}" >&2
    echo "  This installation was not created by an installer or is too old." >&2
    echo "  Please reinstall with:" >&2
    echo "    curl -fsSL https://builds.skaldagent.net/install.sh | bash" >&2
    exit 1
fi

CHANNEL="$(cat "$CHANNEL_FILE" | tr -d '[:space:]')"

# ── Colours (if terminal) ─────────────────────────────────────────────────────
if [ -t 1 ]; then
    RED='\033[0;31m'
    GREEN='\033[0;32m'
    YELLOW='\033[1;33m'
    CYAN='\033[0;36m'
    BOLD='\033[1m'
    NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; CYAN=''; BOLD=''; NC=''
fi

info()  { printf "${GREEN}%s${NC}\n" "$*"; }
warn()  { printf "${YELLOW}⚠ %s${NC}\n" "$*"; }
err()   { printf "${RED}✖ %s${NC}\n" "$*"; }
header(){ printf "\n${BOLD}%s${NC}\n" "$*"; }
banner(){ printf "\n${CYAN}${BOLD}%s${NC}\n" "$*"; }

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

command -v curl >/dev/null 2>&1 || { err "curl is required but not installed."; exit 1; }

# ── Determine download URL ────────────────────────────────────────────────────
BASE_URL="https://builds.skaldagent.net"

case "$CHANNEL" in
    release)
        LATEST="$(curl -fsSL "${BASE_URL}/releases/LATEST" | head -1 | tr -d '[:space:]')"
        if [ -z "$LATEST" ]; then
            err "Could not fetch latest release version from ${BASE_URL}/releases/LATEST"
            exit 1
        fi

        VERSION_FILE="${INSTALL_DIR}/.release-version"
        if [ -f "$VERSION_FILE" ]; then
            CURRENT="$(cat "$VERSION_FILE" | tr -d '[:space:]')"
            if [ "$CURRENT" = "$LATEST" ]; then
                info "✔ Already up to date (${CURRENT})."
                exit 0
            fi
            info "🚀 Update available: ${CURRENT} → ${LATEST}"
        else
            info "🚀 Installing latest release: ${LATEST}"
        fi

        VERSION="$LATEST"
        TARBALL_URL="${BASE_URL}/releases/${VERSION}/skald-circle-${VERSION}-${OS}-${ARCH}.tar.gz"
        DISPLAY_VERSION="$VERSION"
        ;;

    nightly)
        TARBALL_URL="${BASE_URL}/nightly/skald-circle-nightly-${OS}-${ARCH}.tar.gz"
        DISPLAY_VERSION="nightly"
        info "🚀 Updating to latest nightly build …"
        ;;

    *)
        err "Unknown channel in ${CHANNEL_FILE}: '${CHANNEL}'"
        err "Expected 'release' or 'nightly'."
        exit 1
        ;;
esac

# ── Stop service ──────────────────────────────────────────────────────────────
stop_service() {
    case "$OS" in
        Linux)
            if command -v systemctl >/dev/null 2>&1; then
                if systemctl --user is-active skald-circle.service >/dev/null 2>&1; then
                    info "⏹️  Stopping service …"
                    systemctl --user stop skald-circle.service
                fi
            fi
            ;;
        Darwin)
            if command -v launchctl >/dev/null 2>&1; then
                if launchctl list com.skald.circle >/dev/null 2>&1; then
                    info "⏹️  Stopping agent …"
                    launchctl unload "$HOME/Library/LaunchAgents/com.skald.circle.plist" 2>/dev/null || true
                fi
            fi
            ;;
    esac
}

# ── Start service ─────────────────────────────────────────────────────────────
start_service() {
    case "$OS" in
        Linux)
            if command -v systemctl >/dev/null 2>&1; then
                info "▶ Starting service …"
                systemctl --user start skald-circle.service
            fi
            ;;
        Darwin)
            if command -v launchctl >/dev/null 2>&1; then
                info "▶ Starting agent …"
                launchctl load "$HOME/Library/LaunchAgents/com.skald.circle.plist" 2>/dev/null || true
            fi
            ;;
    esac
}

# ── Main ──────────────────────────────────────────────────────────────────────
banner "╔══════════════════════════════════════════╗"
banner "║       Skald Circle — Updater (${DISPLAY_VERSION})     ║"
banner "╚══════════════════════════════════════════╝"
echo ""
echo "  Channel    : ${CHANNEL}"
echo "  Platform   : ${OS}/${ARCH}"
echo "  Install    : ${INSTALL_DIR}"
echo ""

stop_service

# Download & extract
TMP_TARBALL="$(mktemp -t skald-update.XXXXXX.tar.gz)"
trap 'rm -f "$TMP_TARBALL"' EXIT

info "↓ Downloading Skald Circle (${DISPLAY_VERSION}) …"
curl -fsSL -o "$TMP_TARBALL" "$TARBALL_URL"

info "📦 Extracting …"
tar xzf "$TMP_TARBALL" -C "$INSTALL_DIR" --strip-components=1

if [ ! -x "$INSTALL_DIR/bin/skald" ]; then
    err "Extraction failed — skald binary not found."
    exit 1
fi

# Update version file for release channel
if [ "$CHANNEL" = "release" ]; then
    echo "$VERSION" > "$INSTALL_DIR/.release-version"
fi

# Rebuild Python venv (best effort — new deps may have appeared)
info "🔧 Rebuilding Python virtual environment …"
VENV_DIR="${INSTALL_DIR}/.venv"
REQUIREMENTS="${INSTALL_DIR}/requirements.txt"

rm -rf "$VENV_DIR"
if command -v uv >/dev/null 2>&1; then
    uv venv --seed "$VENV_DIR" && uv pip install -r "$REQUIREMENTS" \
        && info "✔ Python venv ready (uv)" \
        || warn "Python venv setup failed — Python MCP servers will be unavailable."
elif command -v python3 >/dev/null 2>&1; then
    python3 -m venv "$VENV_DIR" && "$VENV_DIR/bin/pip" install -r "$REQUIREMENTS" \
        && info "✔ Python venv ready (pip)" \
        || warn "Python venv setup failed — Python MCP servers will be unavailable."
else
    warn "python3 not found — Python MCP servers will be unavailable."
fi

start_service

# ── Done ──────────────────────────────────────────────────────────────────────
echo ""
info "✅ Skald Circle updated to ${DISPLAY_VERSION}!"
echo ""
echo "  Status:  $( [ "$OS" = "linux" ] && echo "systemctl --user status skald-circle" || echo "launchctl list com.skald.circle" )"
echo "  Logs:    $( [ "$OS" = "linux" ] && echo "journalctl --user -u skald-circle -f" || echo "tail -f ${INSTALL_DIR}/logs/stdout.log" )"
echo "  Update:  ${INSTALL_DIR}/update.sh"
echo ""
