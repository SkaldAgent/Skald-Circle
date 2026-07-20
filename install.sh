#!/usr/bin/env sh
# install.sh — install the latest release of Skald Circle
#
# Usage:
#   curl -fsSL https://builds.skaldagent.net/install.sh | bash
#
# Supports Linux (systemd) and macOS ARM64 (launchd).
# Default install dir: ~/.local/share/skald-circle (override with SKALD_DIR).
#
# If Docker is missing, the installer can optionally install it.

set -eu

# ── User overrides ────────────────────────────────────────────────────────────
INSTALL_DIR="${SKALD_DIR:-$HOME/.local/share/skald-circle}"

# ── Detect interactive stdin ──────────────────────────────────────────────────
# We need this early: the confirmation prompt reads from /dev/tty when stdin is
# piped (curl | bash), but we only show it when a terminal is actually available.
if [ -t 0 ]; then
    IS_INTERACTIVE=true
else
    IS_INTERACTIVE=false
fi

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
# ── Network IP helper ──────────────────────────────────────────────────────────
# Returns the primary non-loopback network IP, or empty string if unavailable.
get_network_ip() {
    if command -v ip >/dev/null 2>&1; then
        ip route get 1 2>/dev/null | awk '{print $7}'
    elif command -v ifconfig >/dev/null 2>&1; then
        ifconfig 2>/dev/null | grep -E 'inet ' | grep -v '127.0.0.1' | awk '{print $2}' | head -1
    elif command -v hostname >/dev/null 2>&1; then
        hostname -I 2>/dev/null | awk '{print $1}'
    fi
}



# ── Prompt helpers (work from piped stdin too) ────────────────────────────────
# Both read from /dev/tty when stdin is piped (curl | bash), so the user can
# always see and answer.

prompt_enter() {
    local msg="${1:-Press Enter to continue or Ctrl+C to cancel}"
    if [ "$IS_INTERACTIVE" = true ]; then
        printf "%s" "$msg" >&2
        read -r _ || true
    elif (: </dev/tty) 2>/dev/null; then
        printf "%s" "$msg" >/dev/tty
        IFS= read -r _ </dev/tty || true
    fi
}

prompt_yes_no() {
    local msg="${1:-Continue? [Y/n]}"
    local ans
    if [ "$IS_INTERACTIVE" = true ]; then
        printf "%s" "$msg" >&2
        read -r ans || ans="y"
    elif (: </dev/tty) 2>/dev/null; then
        printf "%s" "$msg" >/dev/tty
        IFS= read -r ans </dev/tty || ans="y"
    else
        ans="n"
    fi
    case "$(printf "%s" "$ans" | tr '[:upper:]' '[:lower:]' | tr -d ' ')" in
        ""|"y"|"yes") return 0 ;;
        *) return 1 ;;
    esac
}

# ── Docker install helper ─────────────────────────────────────────────────────
install_docker() {
    if [ "$OS" = "linux" ]; then
        info "▶ Installing Docker via get.docker.com …"
        curl -fsSL https://get.docker.com | sh
        info "✔ Docker installed"

        # Add current user to docker group so they don't need sudo for every command
        if command -v usermod >/dev/null 2>&1; then
            sudo usermod -aG docker "$USER"
            warn "You may need to log out and back in for the docker group to take effect."
        fi
        info "✔ User added to docker group"
    elif [ "$OS" = "darwin" ]; then
        if command -v brew >/dev/null 2>&1; then
            info "▶ Installing Docker Desktop via Homebrew …"
            brew install --cask docker
            info "✔ Docker Desktop installed. Open it from Applications to complete setup."
        else
            err "Homebrew not found. Please install Docker Desktop manually from:"
            err "  https://docs.docker.com/desktop/setup/install/mac-install/"
            err "Then re-run this installer."
            exit 1
        fi
    fi
}

ask_install_docker() {
    echo ""
    header "🐳 Docker"
    echo ""
    if command -v docker >/dev/null 2>&1 && docker version >/dev/null 2>&1; then
        info "✔ Docker is installed and the daemon is running."
        return 0
    elif command -v docker >/dev/null 2>&1; then
        echo "  Docker CLI is present but the daemon is not running."
        echo "  Please start Docker before starting the server."
        echo ""
        return 0
    else
        warn "Docker is required but not found."
        echo ""
        echo "  Skald uses Docker to run sandboxed user containers."
        echo "  The server will not start without it."
        echo ""
        if prompt_yes_no "  Install Docker now? [Y/n] "; then
            install_docker
            echo ""
        else
            warn "Skipping Docker installation."
            echo "  You can install it later: https://docs.docker.com/engine/install/"
            echo ""
        fi
    fi
}


# ── Optional dependency checks ────────────────────────────────────────────────
# These are just warnings — the server starts without them, but some MCP servers
# or plugins won't work.

check_optional_deps() {
    echo ""
    header "🔧 Optional dependencies"
    echo ""

    if command -v python3 >/dev/null 2>&1; then
        info "✔ Python 3 found ($(python3 --version 2>&1 | head -1))"
    else
        warn "Python 3 not found — Python MCP servers (Gmail, GCal, GMaps, ...) will not work."
        echo "  Install it from https://www.python.org/downloads/"
        echo ""
    fi

    if command -v node >/dev/null 2>&1; then
        info "✔ Node.js found ($(node --version 2>&1 | head -1))"
    else
        warn "Node.js not found — WhatsApp MCP server will not work."
        echo "  Install it from https://nodejs.org/ (version 18 or later)"
        echo ""
    fi
}
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

# ── Banner + fetch latest version ─────────────────────────────────────────────
banner "╔══════════════════════════════════════════╗"
banner "║        Skald Circle — Installer          ║"
banner "╚══════════════════════════════════════════╝"
echo ""

info "🔍 Looking up latest release …"
BASE_URL="https://builds.skaldagent.net"
LATEST_URL="${BASE_URL}/releases/LATEST"
VERSION="$(curl -fsSL "$LATEST_URL" | head -1 | tr -d '[:space:]')"

if [ -z "$VERSION" ]; then
    err "Could not determine latest release version."
    err "Check ${LATEST_URL} or try install-nightly.sh for the latest build."
    exit 1
fi

TARBALL_URL="${BASE_URL}/releases/${VERSION}/skald-circle-${VERSION}-${OS}-${ARCH}.tar.gz"

# ── Summary + confirmation ────────────────────────────────────────────────────
echo ""
header "Installation summary"
echo ""
echo "  What       : Skald Circle ${VERSION}"
echo "  Platform   : ${OS}/${ARCH}"
echo "  Install to : ${INSTALL_DIR}"
echo "  Service    : $( [ "$OS" = "linux" ] && echo "systemd (user)" || echo "launchd" )"
echo ""
echo "  The installer will check for Docker and offer to install it if missing."
echo "  Python 3 and Node.js are optional — needed for some MCP servers."
echo ""

prompt_enter "Press Enter to continue or Ctrl+C to cancel "

# ── Docker check & install ────────────────────────────────────────────────────
ask_install_docker

# ── Optional dependency check ─────────────────────────────────────────────────
check_optional_deps

# ── Download & extract ────────────────────────────────────────────────────────
info "↓ Downloading Skald Circle ${VERSION} …"
mkdir -p "$INSTALL_DIR"
curl -fsSL "$TARBALL_URL" | tar xz -C "$INSTALL_DIR" --strip-components=1

if [ ! -x "$INSTALL_DIR/bin/skald" ]; then
    err "Download or extraction failed — skald binary not found."
    exit 1
fi

info "✔ Extracted to ${INSTALL_DIR}"

# ── Python venv (best-effort) ─────────────────────────────────────────────────
# Create the venv inline instead of calling run.sh (which also launches the
# server and would hang the installer).
info "🔧 Setting up Python virtual environment …"
VENV_DIR="${INSTALL_DIR}/.venv"
REQUIREMENTS="${INSTALL_DIR}/requirements.txt"

# A venv is usable only if python3 AND pip both work. Ubuntu's `python3 -m venv`
# without the python3-venv package leaves a venv with python3 but no pip — detect
# that and recreate, so a broken venv never survives a restart.
if [ ! -f "$VENV_DIR/bin/python3" ] || ! "$VENV_DIR/bin/python3" -m pip --version >/dev/null 2>&1; then
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
else
    info "✔ Python venv already exists"
fi

# ── Install daemon ────────────────────────────────────────────────────────────
if [ "$OS" = "linux" ] && [ -z "${NOSYSTEMD:-}" ]; then
    header "⚡ Installing systemd user service …"

    mkdir -p "$HOME/.config/systemd/user"

    cat > "$HOME/.config/systemd/user/skald-circle.service" <<- SERVICE
[Unit]
Description=Skald Circle (release ${VERSION})
Documentation=https://skaldagent.net
After=network.target docker.service

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

# ── Welcome + first-run setup ─────────────────────────────────────────────────
echo ""
header "👋 Welcome to Skald Circle!"

if [ -x "$INSTALL_DIR/bin/skald-setup" ]; then
    echo ""
    echo "  The server is running. Now let's create your admin account."
    echo "  You'll be asked for a username and password."
    echo ""
    # skald-setup uses the relative path `database/system.db`, so we cd to the
    # install directory first. When running via curl | bash the cwd is ~, which
    # would create `~/database/system.db` instead of the correct location.
    cd "$INSTALL_DIR"
    if [ "$IS_INTERACTIVE" = true ]; then
        "bin/skald-setup"
    elif (: </dev/tty) 2>/dev/null; then
        "bin/skald-setup" </dev/tty
    else
        echo "  From a terminal, run:"
        echo "    cd ${INSTALL_DIR} && ./bin/skald-setup"
    fi
    echo ""
fi

# ── Done ──────────────────────────────────────────────────────────────────────
echo ""
info "✅ Skald Circle ${VERSION} installed successfully!"
echo ""
echo "  Server : ${INSTALL_DIR}/run.sh"
echo "  Binary : ${INSTALL_DIR}/bin/skald"
echo "  Setup  : ${INSTALL_DIR}/bin/skald-setup"
NET_IP="$(get_network_ip)"
ADMIN_URL="${NET_IP:+http://${NET_IP}:9000}"
echo "  Admin console: ${ADMIN_URL:-http://localhost:9000}  (on the machine, or use the IP above from another device)"
echo "  Server IP:    ${NET_IP:-localhost}  (network address if available)"
echo ""
echo ""
echo "  Start:  $( [ "$OS" = "linux" ] && echo "systemctl --user start skald-circle" || echo "launchctl start com.skald.circle" )"
echo "  Stop:   $( [ "$OS" = "linux" ] && echo "systemctl --user stop skald-circle" || echo "launchctl stop com.skald.circle" )"
echo "  Logs:   $( [ "$OS" = "linux" ] && echo "journalctl --user -u skald-circle -f" || echo "tail -f ${INSTALL_DIR}/logs/stdout.log" )"
echo ""
