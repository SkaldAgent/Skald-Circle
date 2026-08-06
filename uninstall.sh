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
# Normalized to lowercase like every sibling script (install.sh, install-nightly.sh,
# update.sh). This file used to match uname's raw `Linux`/`Darwin`, which was
# correct on its own but made the four scripts disagree — and a `case` copied
# between two of them is exactly how update.sh ended up with stop_service and
# start_service as silent no-ops.
case "$(uname -s)" in
    Linux)  OS="linux"  ;;
    Darwin) OS="darwin" ;;
    *)      OS="$(uname -s)" ;;
esac

echo ""
printf "\033[1m🗑️  Skald Circle — Uninstaller\033[0m\n"
echo ""
echo "  This will permanently delete Skald Circle and all its data:"
echo "    ${INSTALL_DIR}"
echo ""

# Confirm before a destructive delete. Read from the terminal even when stdin is
# piped (curl | sh); if there's no terminal at all, require SKALD_YES=1 so an
# install is never wiped with no confirmation.
if [ "${SKALD_YES:-}" = "1" ]; then
    :
elif [ -t 0 ]; then
    printf "%s " "Are you sure? Type 'yes' to continue:"
    read -r CONFIRM
    [ "$CONFIRM" = "yes" ] || { echo "Aborted."; exit 0; }
    echo ""
elif (: </dev/tty) 2>/dev/null; then
    printf "%s " "Are you sure? Type 'yes' to continue:" >/dev/tty
    IFS= read -r CONFIRM </dev/tty
    [ "$CONFIRM" = "yes" ] || { echo "Aborted."; exit 0; }
    echo ""
else
    err "No terminal available for confirmation."
    err "Re-run with SKALD_YES=1 to uninstall non-interactively."
    exit 1
fi

# ── Stop & remove daemon ──────────────────────────────────────────────────────
case "$OS" in
    linux)
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

    darwin)
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

# ── Remove Docker sandboxes ───────────────────────────────────────────────────
# Each user has a container named skald-{userid} that bind-mounts files under the
# install dir. The daemon is stopped above, so the server won't recreate them
# mid-cleanup; removing them here also frees the (sometimes root-owned) mount
# files that would otherwise force the sudo fallback on the rm below.
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
    # Anchored: Docker's name filter is a regex matched anywhere in the name, so
    # an unanchored `skald-` also selects somebody else's `my-skald-proxy` — and
    # the next line is `docker rm -f`. Ours are always `skald-{userid}`.
    CONTAINERS="$(docker ps -aq --filter 'name=^skald-' 2>/dev/null || true)"
    if [ -n "$CONTAINERS" ]; then
        info "🐳 Removing Skald Docker containers …"
        # shellcheck disable=SC2086  # word-splitting is intentional (multiple IDs)
        docker rm -f $CONTAINERS >/dev/null 2>&1 || true
    fi
    IMAGES="$(docker images -q skald-runtime 2>/dev/null || true)"
    if [ -n "$IMAGES" ]; then
        info "🐳 Removing Skald runtime image …"
        # shellcheck disable=SC2086
        docker rmi -f $IMAGES >/dev/null 2>&1 || true
    fi
elif command -v docker >/dev/null 2>&1; then
    warn "Docker daemon not reachable — skipping container cleanup."
    warn "Remove leftovers later with: docker rm -f \$(docker ps -aq --filter name=^skald-)"
fi

# ── Remove installation directory ─────────────────────────────────────────────
if [ -d "$INSTALL_DIR" ]; then
    info "🗑️  Removing installation directory …"
    rm -rf "$INSTALL_DIR" 2>/dev/null || {
        warn "Some files are owned by other users (e.g. from Docker sandboxes)."
        info "🔐 Retrying with sudo …"
        sudo rm -rf "$INSTALL_DIR"
    }
    if [ -d "$INSTALL_DIR" ]; then
        err "Failed to remove ${INSTALL_DIR} even with sudo."
        err "You may need to manually remove it."
        exit 1
    fi
    info "✔ Removed ${INSTALL_DIR}"
else
    warn "Installation directory not found: ${INSTALL_DIR}"
fi

echo ""
info "✅ Skald Circle has been uninstalled."

# The installer enables systemd lingering for this user, which is a persistent
# per-user setting and not ours to take back: any other `systemctl --user`
# service on this box may be relying on it by now, and silently disabling it
# would stop those too. So we say it and leave the choice to the human.
if [ "$OS" = "linux" ] && command -v loginctl >/dev/null 2>&1; then
    case "$(loginctl show-user "${USER:-$(id -un)}" --property=Linger 2>/dev/null || true)" in
        *=yes)
            echo ""
            echo "  Note: systemd lingering is still enabled for ${USER:-$(id -un)}."
            echo "  It was enabled at install so the server survived logout. If no other"
            echo "  user service needs it, turn it off with:"
            echo "      sudo loginctl disable-linger ${USER:-$(id -un)}"
            ;;
    esac
fi

echo ""
echo "  If you want to reinstall:"
echo "    curl -fsSL https://builds.skaldagent.net/install.sh | bash"
echo "    curl -fsSL https://builds.skaldagent.net/install-nightly.sh | bash"
echo ""
