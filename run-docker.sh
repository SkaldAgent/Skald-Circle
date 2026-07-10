#!/usr/bin/env sh
# Docker supervisor loop.
# Runs the pre-built binary directly. On exit -1 (self-rewrite request),
# rebuilds with cargo and restarts. On clean exit (0), stops.

set -u

cd "$(dirname "$0")"

# ── Python venv setup (optional) ─────────────────────────────────────────────
VENV_DIR=".venv"
REQUIREMENTS="requirements.txt"

if [ ! -f "$VENV_DIR/bin/python3" ] && [ -f "$REQUIREMENTS" ]; then
    if command -v uv >/dev/null 2>&1; then
        echo "[run-docker.sh] Setting up Python venv with uv …"
        uv venv "$VENV_DIR" && uv pip install -r "$REQUIREMENTS" \
            && echo "[run-docker.sh] Python venv ready." \
            || echo "[run-docker.sh] Warning: Python venv setup failed — Python MCP servers will be unavailable."
    elif command -v python3 >/dev/null 2>&1; then
        echo "[run-docker.sh] Setting up Python venv …"
        python3 -m venv "$VENV_DIR" && "$VENV_DIR/bin/pip" install -r "$REQUIREMENTS" \
            && echo "[run-docker.sh] Python venv ready." \
            || echo "[run-docker.sh] Warning: Python venv setup failed — Python MCP servers will be unavailable."
    fi
fi

if [ -f "$VENV_DIR/bin/python3" ]; then
    export PATH="$(pwd)/$VENV_DIR/bin:$PATH"
fi

export TS_RS_EXPERIMENT=this_is_unstable_software

BINARY="./skald"

while true; do
    "$BINARY"
    code=$?

    if [ "$code" -eq 0 ]; then
        echo "[run-docker.sh] App exited cleanly. Stopping."
        exit 0
    elif [ "$code" -eq 255 ]; then
        echo "[run-docker.sh] App requested restart (exit -1). Rebuilding…"
        RUSTFLAGS="-A warnings" cargo build --release --no-default-features \
            && cp target/release/skald "$BINARY"
    else
        echo "[run-docker.sh] App exited with code $code. Stopping."
        exit "$code"
    fi
done
