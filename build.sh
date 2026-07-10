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

OUT_DIR="bin"

PROFILE="release"
if [ "${1:-}" = "-d" ]; then
    PROFILE="debug"
    shift
fi

# Warnings are noise in a tight build/restart loop; errors still fail the build.
RUSTFLAGS="-A warnings"
export RUSTFLAGS

# The server takes any forwarded args (e.g. --features desktop); the setup wizard
# is a plain binary and is always built on its own, without them.
if [ "$PROFILE" = "release" ]; then
    cargo build --release "$@"
    cargo build --release -p skald-setup
else
    cargo build "$@"
    cargo build -p skald-setup
fi

# Stage as .new and rename into place: a plain cp over a live binary fails with
# ETXTBSY on Linux while run.sh supervises a running instance.
install_bin() {
    src="target/$PROFILE/$1"
    if [ ! -f "$src" ]; then
        echo "[build.sh] Expected binary not found at $src" >&2
        exit 1
    fi
    cp "$src" "$OUT_DIR/$1.new"
    chmod 755 "$OUT_DIR/$1.new"
    mv -f "$OUT_DIR/$1.new" "$OUT_DIR/$1"
    echo "[build.sh] $PROFILE build installed → $OUT_DIR/$1"
}

mkdir -p "$OUT_DIR"
install_bin skald
install_bin skald-setup
