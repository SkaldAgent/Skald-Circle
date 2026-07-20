#!/usr/bin/env sh
# Build, package, and deploy Skald Circle for macOS (ARM64).
#
# Usage:  ./ci/package-macos.sh
#
# Behaviour depends on the current git branch:
#   release  → builds a release tarball, checks version uniqueness, uploads + updates LATEST
#   main     → builds a nightly tarball, uploads to nightly/ (no version check)
#   other    → aborts with an error
#
# Prerequisites:
#   - macOS ARM64 (Apple Silicon)
#   - SSH alias "skaldserver" configured in ~/.ssh/config pointing to the builds host
#   - ssh + scp working to skaldserver (key-based auth)
#   - ci/package.sh, ci/verify-version.sh in the repo

set -eu

cd "$(dirname "$0")/.."

# ── Config ──────────────────────────────────────────────────────────────────
REMOTE_HOST="skaldserver"
REMOTE_BASE="/var/www/builds.skaldagent.net"
BUILDS_URL="https://builds.skaldagent.net"

# ── Detect branch ────────────────────────────────────────────────────────────
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
echo "[package-macos] Branch: ${BRANCH}"

case "$BRANCH" in
    release)
        MODE="release"
        ;;
    main)
        MODE="nightly"
        ;;
    *)
        echo "[package-macos] ❌ Aborting: must be on 'release' or 'main' branch (current: ${BRANCH})"
        exit 1
        ;;
esac

# ── Version ──────────────────────────────────────────────────────────────────
if [ "$MODE" = "release" ]; then
    VERSION="v$(grep '^version ' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')"
    echo "[package-macos] Release version: ${VERSION}"
else
    VERSION="nightly"
    echo "[package-macos] Nightly build"
fi

# ── Verify version is new (release only) ────────────────────────────────────
if [ "$MODE" = "release" ]; then
    echo "[package-macos] Checking if release ${VERSION} already exists on remote..."
    REMOTE_DIR_URL="${BUILDS_URL}/releases/${VERSION}/"
    if curl -I --fail --silent --output /dev/null "$REMOTE_DIR_URL" 2>/dev/null; then
        echo "[package-macos] ❌ Release ${VERSION} already exists at ${REMOTE_DIR_URL}"
        echo "[package-macos]    Bump the version in Cargo.toml before releasing."
        exit 1
    fi
    echo "[package-macos] ✅ Release ${VERSION} is new — proceeding."
fi

# ── Build ────────────────────────────────────────────────────────────────────
echo "[package-macos] Building (this will take a while)..."
cargo build --release
cargo build --release -p skald-setup
echo "[package-macos] ✅ Build complete."

# ── Package ──────────────────────────────────────────────────────────────────
echo "[package-macos] Packaging..."
mkdir -p dist
if [ "$MODE" = "release" ]; then
    ./ci/package.sh \
        --version "$VERSION" \
        --os darwin \
        --arch arm64 \
        --target-dir target/release \
        --output dist/
else
    ./ci/package.sh \
        --version nightly \
        --os darwin \
        --arch arm64 \
        --target-dir target/release \
        --output dist/
fi

# ── Upload via SCP ───────────────────────────────────────────────────────────
echo "[package-macos] Uploading to ${REMOTE_HOST}..."

if [ "$MODE" = "release" ]; then
    # Create remote directory and copy tarball
    ssh "$REMOTE_HOST" "mkdir -p ${REMOTE_BASE}/releases/${VERSION}"
    scp dist/skald-circle-${VERSION}-darwin-arm64.tar.gz \
        "${REMOTE_HOST}:${REMOTE_BASE}/releases/${VERSION}/"

    # Update LATEST pointer
    echo "$VERSION" | ssh "$REMOTE_HOST" "cat > ${REMOTE_BASE}/releases/LATEST"
    echo "[package-macos] ✅ Release ${VERSION} deployed + LATEST updated."
else
    # Nightly — copy into nightly/ directory
    ssh "$REMOTE_HOST" "mkdir -p ${REMOTE_BASE}/nightly"
    scp dist/skald-circle-nightly-darwin-arm64.tar.gz \
        "${REMOTE_HOST}:${REMOTE_BASE}/nightly/"
    echo "[package-macos] ✅ Nightly deployed."
fi

# ── Summary ──────────────────────────────────────────────────────────────────
echo ""
echo "[package-macos] ─────────────────────────────────────────────"
echo "[package-macos]   Mode:    ${MODE}"
echo "[package-macos]   Version: ${VERSION}"
echo "[package-macos]   Branch:  ${BRANCH}"
echo "[package-macos]   Remote:  ${REMOTE_HOST}"
echo "[package-macos] ─────────────────────────────────────────────"
echo "[package-macos] ✅ Done."
