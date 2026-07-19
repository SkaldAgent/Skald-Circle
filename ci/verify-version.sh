#!/usr/bin/env sh
# Verify that a Skald Circle release version has not been built yet.
#
# Intended as a required Gitea Actions status check on PRs to the `release`
# branch. Runs in the repo root after checkout.
#
# Usage:
#   ./scripts/verify-version.sh \
#       --builds-dir /var/www/builds.skaldagent.net
#
# Exit codes:
#   0  → version is new (or builds-dir doesn't exist yet) → PR may proceed
#   1  → version already built → PR should fail
#
# Reads the version from Cargo.toml in the current directory.

set -eu

# ── Parse args ────────────────────────────────────────────────────────────────
BUILDS_DIR=""

while [ $# -gt 0 ]; do
    case "$1" in
        --builds-dir) BUILDS_DIR="$2"; shift 2 ;;
        *) echo "[verify-version] Unknown option: $1" >&2; exit 1 ;;
    esac
done

if [ -z "$BUILDS_DIR" ]; then
    echo "[verify-version] Missing --builds-dir" >&2
    exit 1
fi

# ── Read version from Cargo.toml ──────────────────────────────────────────────
# This is the workspace root's Cargo.toml.
VERSION="$(grep '^version ' Cargo.toml | head -1 | sed 's/version *= *"\(.*\)"/\1/')"

if [ -z "$VERSION" ]; then
    echo "[verify-version] ERROR: Could not read version from Cargo.toml" >&2
    exit 1
fi

echo "[verify-version] Version in Cargo.toml: v${VERSION}"

# ── Check if already built ────────────────────────────────────────────────────
RELEASE_DIR="${BUILDS_DIR}/releases/v${VERSION}"

if [ -d "$RELEASE_DIR" ]; then
    echo "[verify-version] ❌ Release v${VERSION} already exists at:"
    echo "[verify-version]    ${RELEASE_DIR}"
    echo "[verify-version]    Bump the version in Cargo.toml before merging."
    exit 1
fi

echo "[verify-version] ✅ Release v${VERSION} is new — no conflict."
exit 0
