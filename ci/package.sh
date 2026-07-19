#!/usr/bin/env sh
# Package a Skald Circle build into a distributable tarball.
#
# Usage:
#   ./scripts/package.sh \
#       --version v0.1.0 \
#       --arch amd64 \
#       --target-dir target/release \
#       --output /tmp/dist
#
#   --version   Version string, e.g. "v0.1.0" or "nightly"
#   --arch      Architecture: "amd64" or "arm64"
#   --target-dir  Path to cargo release output (target/release or
#                 target/aarch64-unknown-linux-gnu/release)
#   --output    Directory where the .tar.gz will be written
#
# The tarball contains everything needed to run Skald Circle:
#   bin/skald, bin/skald-setup, web/, agents/, skills/,
#   default.config.yaml, requirements.txt, run.sh

set -eu

cd "$(dirname "$0")/.."

# ── Parse args ────────────────────────────────────────────────────────────────
VERSION=""
ARCH=""
TARGET_DIR=""
OUTPUT=""

while [ $# -gt 0 ]; do
    case "$1" in
        --version)    VERSION="$2";    shift 2 ;;
        --arch)       ARCH="$2";       shift 2 ;;
        --target-dir) TARGET_DIR="$2"; shift 2 ;;
        --output)     OUTPUT="$2";     shift 2 ;;
        *) echo "[package.sh] Unknown option: $1" >&2; exit 1 ;;
    esac
done

if [ -z "$VERSION" ] || [ -z "$ARCH" ] || [ -z "$TARGET_DIR" ] || [ -z "$OUTPUT" ]; then
    echo "[package.sh] Missing required argument. See usage." >&2
    exit 1
fi

PACKAGE_NAME="skald-circle-${VERSION}-linux-${ARCH}"
STAGING="$(mktemp -d)/${PACKAGE_NAME}"
mkdir -p "$STAGING/bin"

echo "[package.sh] Packaging $PACKAGE_NAME"
echo "[package.sh]   target-dir:  $TARGET_DIR"
echo "[package.sh]   output:      $OUTPUT"

# ── Verify binaries exist ─────────────────────────────────────────────────────
if [ ! -f "$TARGET_DIR/skald" ]; then
    echo "[package.sh] ERROR: skald binary not found at $TARGET_DIR/skald" >&2
    exit 1
fi
if [ ! -f "$TARGET_DIR/skald-setup" ]; then
    echo "[package.sh] ERROR: skald-setup binary not found at $TARGET_DIR/skald-setup" >&2
    exit 1
fi

# ── Copy binaries (stripped) ──────────────────────────────────────────────────
cp "$TARGET_DIR/skald"       "$STAGING/bin/skald"
cp "$TARGET_DIR/skald-setup" "$STAGING/bin/skald-setup"
strip "$STAGING/bin/skald" "$STAGING/bin/skald-setup"
chmod 755 "$STAGING/bin/skald" "$STAGING/bin/skald-setup"

# ── Copy runtime assets ───────────────────────────────────────────────────────
cp -r web              "$STAGING/web"
cp -r agents           "$STAGING/agents"
cp -r skills           "$STAGING/skills"
cp default.config.yaml "$STAGING/default.config.yaml"
cp requirements.txt    "$STAGING/requirements.txt"
cp run.sh              "$STAGING/run.sh"
chmod 755 "$STAGING/run.sh"

# ── Create tarball ────────────────────────────────────────────────────────────
mkdir -p "$OUTPUT"
TARBALL="${OUTPUT}/${PACKAGE_NAME}.tar.gz"

cd "$(dirname "$STAGING")"
tar czf "$TARBALL" "$PACKAGE_NAME"
cd - > /dev/null

rm -rf "$(dirname "$STAGING")"

SHA256="$(sha256sum "$TARBALL" | cut -d' ' -f1)"
echo "[package.sh] ✅ Created $TARBALL"
echo "[package.sh]    sha256: $SHA256"
echo "[package.sh]    size:   $(du -h "$TARBALL" | cut -f1)"
