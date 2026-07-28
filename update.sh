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
# Robustness notes (why the flow looks the way it does):
#   * The tarball is downloaded and validated in a *staging* directory BEFORE
#     the running service is touched — a broken download never takes the app
#     down.
#   * The service is stopped and we WAIT until the process is actually gone
#     before extracting: overwriting a live binary in place fails with ETXTBSY
#     (Linux) or on a running Mach-O (macOS), which would abort the update with
#     the service left down.
#   * The whole flow runs inside main(), invoked on the very last line, so the
#     shell has parsed the entire script into memory before `tar` overwrites
#     update.sh with its own new copy (the tarball ships this script). Without
#     this, the shell would read garbage for the tail and never restart.
#   * A trap restarts the service if the update fails after the stop, so a
#     failed update never leaves the box down.

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

CHANNEL="$(tr -d '[:space:]' < "$CHANNEL_FILE")"

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

BASE_URL="https://builds.skaldagent.net"

# ── Global state (referenced by the EXIT trap) ────────────────────────────────
TMP_TARBALL=""
STAGING=""
STOPPED=0   # set once the service has been stopped
STARTED=0   # set once it has been (re)started

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

# ── Wait until the server process is actually gone ────────────────────────────
# systemctl stop is synchronous, but launchctl unload kills the process group
# asynchronously — so we poll for the real binary before overwriting it.
wait_until_stopped() {
    # Match the server binary by path prefix. Deliberately unanchored: the
    # server runs with no args today, but we'd rather over-match (a harmless
    # extra wait) than miss a still-running process and race the extraction.
    # `skald-setup` only runs at first-run setup, never during an update.
    pat="${INSTALL_DIR}/bin/skald"
    if command -v pgrep >/dev/null 2>&1; then
        i=0
        while pgrep -f "$pat" >/dev/null 2>&1; do
            i=$((i + 1))
            if [ "$i" -gt 20 ]; then
                warn "Server still running after ~20s; proceeding anyway."
                return 0
            fi
            sleep 1
        done
    else
        # No pgrep: give the service manager a moment to tear the process down.
        sleep 3
    fi
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

# ── Cleanup + safety net ──────────────────────────────────────────────────────
# Runs on every exit. Removes temp files and, if the update died after the
# service was stopped but before it came back up, makes a best-effort restart so
# the box is never left down.
cleanup() {
    [ -n "${TMP_TARBALL:-}" ] && rm -f "$TMP_TARBALL" 2>/dev/null || true
    [ -n "${STAGING:-}" ] && rm -rf "$STAGING" 2>/dev/null || true
    if [ "$STOPPED" = "1" ] && [ "$STARTED" != "1" ]; then
        warn "Update failed after the service was stopped — attempting to restart it …"
        start_service || true
    fi
}
trap cleanup EXIT

# ── Main ──────────────────────────────────────────────────────────────────────
main() {
    # ── Resolve download URL + version (may exit early if already current) ─────
    case "$CHANNEL" in
        release)
            LATEST="$(curl -fsSL "${BASE_URL}/releases/LATEST" | head -1 | tr -d '[:space:]')"
            if [ -z "$LATEST" ]; then
                err "Could not fetch latest release version from ${BASE_URL}/releases/LATEST"
                exit 1
            fi

            VERSION_FILE="${INSTALL_DIR}/.release-version"
            if [ -f "$VERSION_FILE" ]; then
                CURRENT="$(tr -d '[:space:]' < "$VERSION_FILE")"
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

    banner "╔══════════════════════════════════════════╗"
    banner "║       Skald Circle — Updater (${DISPLAY_VERSION})     ║"
    banner "╚══════════════════════════════════════════╝"
    echo ""
    echo "  Channel    : ${CHANNEL}"
    echo "  Platform   : ${OS}/${ARCH}"
    echo "  Install    : ${INSTALL_DIR}"
    echo ""

    # ── Download + validate BEFORE touching the running service ────────────────
    # A broken download or a bad archive must never take the app down: we only
    # stop the service once we hold a known-good tarball.
    TMP_TARBALL="$(mktemp -t skald-update.XXXXXX.tar.gz)"

    info "↓ Downloading Skald Circle (${DISPLAY_VERSION}) …"
    curl -fsSL -o "$TMP_TARBALL" "$TARBALL_URL"

    info "🔎 Verifying archive …"
    STAGING="$(mktemp -d -t skald-update-staging.XXXXXX)"
    tar xzf "$TMP_TARBALL" -C "$STAGING" --strip-components=1
    if [ ! -x "$STAGING/bin/skald" ]; then
        err "Downloaded archive is invalid — skald binary not found."
        err "The running server was left untouched."
        exit 1
    fi

    # ── Stop the service and wait for the process to actually exit ─────────────
    stop_service
    STOPPED=1
    wait_until_stopped

    # ── Install (binary is no longer busy) ─────────────────────────────────────
    info "📦 Installing update …"
    tar xzf "$TMP_TARBALL" -C "$INSTALL_DIR" --strip-components=1

    if [ ! -x "$INSTALL_DIR/bin/skald" ]; then
        err "Extraction failed — skald binary not found."
        exit 1
    fi

    # Update version file for release channel
    if [ "$CHANNEL" = "release" ]; then
        echo "$VERSION" > "$INSTALL_DIR/.release-version"
    fi

    # ── Rebuild Python venv (best effort — new deps may have appeared) ─────────
    info "🔧 Rebuilding Python virtual environment …"
    VENV_DIR="${INSTALL_DIR}/.venv"
    REQUIREMENTS="${INSTALL_DIR}/requirements.txt"

    rm -rf "$VENV_DIR"
    if command -v uv >/dev/null 2>&1; then
        uv venv --seed "$VENV_DIR" && uv pip install -r "$REQUIREMENTS" \
            && info "✔ Python venv ready (uv)" \
            || warn "Python venv setup failed — the TTS plugins and host-run connectors will be unavailable."
    elif command -v python3 >/dev/null 2>&1; then
        python3 -m venv "$VENV_DIR" && "$VENV_DIR/bin/pip" install -r "$REQUIREMENTS" \
            && info "✔ Python venv ready (pip)" \
            || warn "Python venv setup failed — the TTS plugins and host-run connectors will be unavailable."
    else
        warn "python3 not found — the TTS plugins and host-run connectors will be unavailable."
    fi

    # ── Restart ────────────────────────────────────────────────────────────────
    start_service
    STARTED=1

    # ── Hint about optional deps ───────────────────────────────────────────────
    if [ -f "${INSTALL_DIR}/requirements-optional.txt" ]; then
        info "💡 Optional GPU/ML dependencies available:"
        info "   ${INSTALL_DIR}/requirements-optional.txt"
        echo "   Install them manually if you use the Orpheus TTS plugin:"
        echo "     cd ${INSTALL_DIR} && .venv/bin/pip install -r requirements-optional.txt"
        echo ""
    fi

    # ── Done ───────────────────────────────────────────────────────────────────
    echo ""
    info "✅ Skald Circle updated to ${DISPLAY_VERSION}!"
    echo ""
    echo "  Status:  $( [ "$OS" = "linux" ] && echo "systemctl --user status skald-circle" || echo "launchctl list com.skald.circle" )"
    echo "  Logs:    $( [ "$OS" = "linux" ] && echo "journalctl --user -u skald-circle -f" || echo "tail -f ${INSTALL_DIR}/logs/stdout.log" )"
    echo "  Update:  ${INSTALL_DIR}/update.sh"
    echo ""
}

# Invoked on the very last line: by the time `tar` overwrites this file on disk
# (the tarball ships update.sh), the shell has already parsed the whole script,
# so the restart tail always runs. Do not add executable code below this line.
main "$@"
