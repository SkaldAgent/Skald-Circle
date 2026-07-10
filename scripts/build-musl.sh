#!/usr/bin/env sh
# Build a fully static Linux binary (musl) without any host cross-toolchain.
#
# Since openssl is gone (rustls) and the crypto backend is `ring` (no OpenSSL /
# aws-lc cmake build), the only native code left to cross-compile is SQLite
# (bundled) and the tree-sitter C grammars — both of which the musl-cross image
# handles out of the box. `whisper-local` (whisper.cpp, C++) is dropped via
# --no-default-features because it is heavy and irrelevant to a headless server;
# set FEATURES="" to include it.
#
# Requirements: Docker. No Rust/musl toolchain needed on the host.
#
# Usage:
#   scripts/build-musl.sh                          # x86_64 static binary
#   TARGET=aarch64-unknown-linux-musl \
#     IMAGE=messense/rust-musl-cross:aarch64-musl \
#     scripts/build-musl.sh                        # arm64 static binary
#
# Output: target/musl/<target>/release/skald
set -eu

TARGET="${TARGET:-x86_64-unknown-linux-musl}"
IMAGE="${IMAGE:-messense/rust-musl-cross:x86_64-musl}"
# Word-splitting is intentional so callers can pass multiple flags.
FEATURES="${FEATURES:---no-default-features}"

PROJ="$(cd "$(dirname "$0")/.." && pwd)"

echo "[build-musl] target=$TARGET image=$IMAGE features='$FEATURES'"

# A dedicated CARGO_TARGET_DIR keeps musl artifacts from clashing with the host
# (macOS) build cache; a named volume caches the crates.io registry across runs.
docker run --rm -t \
    -v "$PROJ":/home/rust/src \
    -v skald-musl-registry:/root/.cargo/registry \
    -e CARGO_TARGET_DIR=/home/rust/src/target/musl \
    "$IMAGE" \
    cargo build --release --target "$TARGET" $FEATURES --bin skald

BIN="$PROJ/target/musl/$TARGET/release/skald"
echo "[build-musl] built: $BIN"
file "$BIN" 2>/dev/null || true
echo "[build-musl] copy this single file to the server and run it — no shared libs required."
