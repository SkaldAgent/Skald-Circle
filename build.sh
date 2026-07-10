#!/usr/bin/env sh
# Build Skald and install the binary into ./bin.
#
#   ./build.sh                     release build  → bin/skald
#   ./build.sh -d                  debug build    → bin/skald
#   ./build.sh --features desktop  extra args are forwarded to cargo
#
# The binary is staged as bin/skald.new and renamed into place. A plain `cp`
# over a live binary fails with ETXTBSY on Linux, and rebuilding while run.sh
# supervises a running instance is the normal workflow: build here, then ask
# the agent to restart — the supervisor re-executes the new binary.

set -eu

cd "$(dirname "$0")"

BIN_NAME="skald"
OUT_DIR="bin"

PROFILE="release"
if [ "${1:-}" = "-d" ]; then
    PROFILE="debug"
    shift
fi

# Warnings are noise in a tight build/restart loop; errors still fail the build.
RUSTFLAGS="-A warnings"
export RUSTFLAGS

if [ "$PROFILE" = "release" ]; then
    cargo build --release "$@"
else
    cargo build "$@"
fi

SRC="target/$PROFILE/$BIN_NAME"
if [ ! -f "$SRC" ]; then
    echo "[build.sh] Expected binary not found at $SRC" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"
cp "$SRC" "$OUT_DIR/$BIN_NAME.new"
chmod 755 "$OUT_DIR/$BIN_NAME.new"
mv -f "$OUT_DIR/$BIN_NAME.new" "$OUT_DIR/$BIN_NAME"

echo "[build.sh] $PROFILE build installed → $OUT_DIR/$BIN_NAME"
