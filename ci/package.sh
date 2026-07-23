#!/usr/bin/env sh
# Package a Skald Circle build into a distributable tarball.
#
# Usage:
#   ./ci/package.sh \
#       --version v0.1.0 \
#       --os linux \
#       --arch amd64 \
#       --target-dir target/release \
#       --output /tmp/dist
#
#   --version    Version string, e.g. "v0.1.0" or "nightly"
#   --os         Target OS: "linux" or "darwin"
#   --arch       Architecture: "amd64" or "arm64"
#   --target-dir Path to cargo release output
#   --output     Directory where the .tar.gz will be written
#
# The tarball contains everything needed to run (or uninstall) Skald Circle:
#   bin/skald, bin/skald-setup, web/, agents/, commands/, skills/, docs/,
#   default.config.yaml, providers.yaml, requirements.txt,
#   requirements-optional.txt, run.sh, update.sh, uninstall.sh

set -eu

cd "$(dirname "$0")/.."

# ── Parse args ────────────────────────────────────────────────────────────────
VERSION=""
OS=""
ARCH=""
TARGET_DIR=""
OUTPUT=""

while [ $# -gt 0 ]; do
    case "$1" in
        --version)    VERSION="$2";    shift 2 ;;
        --os)         OS="$2";         shift 2 ;;
        --arch)       ARCH="$2";       shift 2 ;;
        --target-dir) TARGET_DIR="$2"; shift 2 ;;
        --output)     OUTPUT="$2";     shift 2 ;;
        *) echo "[package.sh] Unknown option: $1" >&2; exit 1 ;;
    esac
done

if [ -z "$VERSION" ] || [ -z "$OS" ] || [ -z "$ARCH" ] || [ -z "$TARGET_DIR" ] || [ -z "$OUTPUT" ]; then
    echo "[package.sh] Missing required argument. See usage." >&2
    exit 1
fi

case "$OS" in
    linux|darwin) ;;
    *) echo "[package.sh] Unsupported OS: $OS (use linux or darwin)" >&2; exit 1 ;;
esac

PACKAGE_NAME="skald-circle-${VERSION}-${OS}-${ARCH}"
STAGING="$(mktemp -d)/${PACKAGE_NAME}"
mkdir -p "$STAGING/bin"

echo "[package.sh] Packaging $PACKAGE_NAME"
echo "[package.sh]   target-dir: $TARGET_DIR"
echo "[package.sh]   output:     $OUTPUT"

# ── Verify binaries exist ─────────────────────────────────────────────────────
if [ ! -f "$TARGET_DIR/skald" ]; then
    echo "[package.sh] ERROR: skald binary not found at $TARGET_DIR/skald" >&2
    exit 1
fi
if [ ! -f "$TARGET_DIR/skald-setup" ]; then
    echo "[package.sh] ERROR: skald-setup binary not found at $TARGET_DIR/skald-setup" >&2
    exit 1
fi

# ── Copy binaries (stripped, best-effort on darwin) ───────────────────────────
cp "$TARGET_DIR/skald"       "$STAGING/bin/skald"
cp "$TARGET_DIR/skald-setup" "$STAGING/bin/skald-setup"

if [ "$OS" = "darwin" ]; then
    # On macOS: strip via xcrun or the system strip (skip if cross-compiled)
    if command -v xcrun >/dev/null 2>&1; then
        xcrun strip "$STAGING/bin/skald" "$STAGING/bin/skald-setup" 2>/dev/null || true
    elif command -v strip >/dev/null 2>&1; then
        strip "$STAGING/bin/skald" "$STAGING/bin/skald-setup" 2>/dev/null || true
    fi
elif [ "$ARCH" = "arm64" ]; then
    STRIP="aarch64-linux-gnu-strip"
    $STRIP "$STAGING/bin/skald" "$STAGING/bin/skald-setup"
else
    strip "$STAGING/bin/skald" "$STAGING/bin/skald-setup"
fi
chmod 755 "$STAGING/bin/skald" "$STAGING/bin/skald-setup"

# ── Copy runtime assets ───────────────────────────────────────────────────────
cp -r web              "$STAGING/web"
cp -r agents           "$STAGING/agents"
cp -r commands         "$STAGING/commands"
cp -r skills           "$STAGING/skills"
cp -r docs             "$STAGING/docs"
cp default.config.yaml "$STAGING/default.config.yaml"
cp providers.yaml      "$STAGING/providers.yaml"
cp requirements.txt    "$STAGING/requirements.txt"
cp requirements-optional.txt "$STAGING/requirements-optional.txt"
cp run.sh              "$STAGING/run.sh"
cp update.sh            "$STAGING/update.sh"
cp uninstall.sh         "$STAGING/uninstall.sh"
chmod 755 "$STAGING/run.sh" "$STAGING/update.sh" "$STAGING/uninstall.sh"
# ── Create tarball ────────────────────────────────────────────────────────────
mkdir -p "$OUTPUT"
TARBALL="$(cd "$OUTPUT" && pwd)/${PACKAGE_NAME}.tar.gz"

cd "$(dirname "$STAGING")"
tar czf "$TARBALL" "$PACKAGE_NAME"
cd - > /dev/null

rm -rf "$(dirname "$STAGING")"

if command -v sha256sum >/dev/null 2>&1; then
    SHA256="$(sha256sum "$TARBALL" | cut -d' ' -f1)"
    echo "[package.sh] ✅ Created $TARBALL"
    echo "[package.sh]    sha256: $SHA256"
    echo "[package.sh]    size:   $(du -h "$TARBALL" | cut -f1)"
elif command -v shasum >/dev/null 2>&1; then
    SHA256="$(shasum -a 256 "$TARBALL" | cut -d' ' -f1)"
    echo "[package.sh] ✅ Created $TARBALL"
    echo "[package.sh]    sha256: $SHA256"
    echo "[package.sh]    size:   $(du -h "$TARBALL" | cut -f1)"
else
    echo "[package.sh] ✅ Created $TARBALL"
    echo "[package.sh]    size:   $(du -h "$TARBALL" | cut -f1)"
fi
