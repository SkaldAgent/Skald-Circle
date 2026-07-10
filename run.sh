#!/usr/bin/env sh
# Supervisor loop for Skald.
#
# Runs a pre-built binary — it never compiles. Build with ./build.sh first:
#
#     ./build.sh && ./run.sh
#
# App exit codes:
#   0    graceful shutdown (SIGINT/SIGTERM) → stop the loop
#   255  restart requested (`restart` tool → libc::_exit(-1)) → re-exec
#   *    error → propagate and stop
#
# The loop re-executes the binary *by path*, so running ./build.sh while the
# supervisor is up and then asking the agent to restart loads the new build.
# Note that `restart` alone no longer rebuilds: edit source, ./build.sh, restart.

set -u

cd "$(dirname "$0")"

# ── Locate the binary ────────────────────────────────────────────────────────
# $SKALD_BIN wins; otherwise prefer the installed bin/, falling back to a plain
# `cargo build --release` output so an existing dev tree keeps working.
if [ -n "${SKALD_BIN:-}" ]; then
    BIN="$SKALD_BIN"
elif [ -x "bin/skald" ]; then
    BIN="bin/skald"
elif [ -x "target/release/skald" ]; then
    BIN="target/release/skald"
else
    echo "[run.sh] No Skald binary found. Build one first:" >&2
    echo "[run.sh]     ./build.sh" >&2
    exit 1
fi

if [ ! -x "$BIN" ]; then
    echo "[run.sh] $BIN is not executable." >&2
    exit 1
fi

# Splitting build from run makes "I forgot to rebuild" the obvious failure mode.
if [ -n "$(find src crates Cargo.toml -newer "$BIN" 2>/dev/null | head -n 1)" ]; then
    echo "[run.sh] Warning: sources are newer than $BIN — run ./build.sh to pick them up."
fi

# ── Python venv setup (optional) ─────────────────────────────────────────────
# Creates .venv/ and installs requirements.txt if Python is available.
# If Python is not installed, the app starts normally but Python-based MCP
# servers (e.g. Gmail, Google Calendar) will fail to connect.
VENV_DIR=".venv"
REQUIREMENTS="requirements.txt"

if [ ! -f "$VENV_DIR/bin/python3" ]; then
    if command -v uv >/dev/null 2>&1; then
        echo "[run.sh] Setting up Python venv with uv …"
        uv venv "$VENV_DIR" && uv pip install -r "$REQUIREMENTS" \
            && echo "[run.sh] Python venv ready." \
            || echo "[run.sh] Warning: Python venv setup failed — Python MCP servers will be unavailable."
    elif command -v python3 >/dev/null 2>&1; then
        echo "[run.sh] Setting up Python venv …"
        python3 -m venv "$VENV_DIR" && "$VENV_DIR/bin/pip" install -r "$REQUIREMENTS" \
            && echo "[run.sh] Python venv ready." \
            || echo "[run.sh] Warning: Python venv setup failed — Python MCP servers will be unavailable."
    else
        echo "[run.sh] Warning: python3 not found — Python MCP servers will be unavailable."
    fi
fi

# If the venv was created, prepend it to PATH so every child process resolves
# python3 to the venv automatically (MCP servers, agent shell commands, etc.).
if [ -f "$VENV_DIR/bin/python3" ]; then
    export PATH="$(pwd)/$VENV_DIR/bin:$PATH"
fi

# ── First-run setup ──────────────────────────────────────────────────────────
# Runs once before the server loop. It decides for itself whether there is work:
# it prompts only when no user exists and stdin is a terminal, and is otherwise a
# no-op. Located next to the server binary; $SKALD_SETUP_BIN overrides.
if [ -n "${SKALD_SETUP_BIN:-}" ]; then
    SETUP_BIN="$SKALD_SETUP_BIN"
else
    SETUP_BIN="$(dirname "$BIN")/skald-setup"
    [ -x "$SETUP_BIN" ] || SETUP_BIN="bin/skald-setup"
    [ -x "$SETUP_BIN" ] || SETUP_BIN="target/release/skald-setup"
fi

if [ -x "$SETUP_BIN" ]; then
    "$SETUP_BIN"
    setup_code=$?
    # A non-zero exit means the wizard failed or the person cancelled it (Ctrl-D).
    # Don't launch a half-configured instance behind their back — stop the loop.
    if [ "$setup_code" -ne 0 ]; then
        echo "[run.sh] Setup did not complete (exit $setup_code). Not starting." >&2
        exit "$setup_code"
    fi
else
    echo "[run.sh] Note: skald-setup not found — skipping first-run setup."
fi

echo "[run.sh] Supervising $BIN"

while true; do
    "$BIN"
    code=$?

    if [ "$code" -eq 0 ]; then
        echo "[run.sh] App exited cleanly. Stopping."
        exit 0
    elif [ "$code" -eq 255 ]; then
        echo "[run.sh] App requested restart (exit -1). Re-executing $BIN …"
        continue
    else
        echo "[run.sh] App exited with code $code. Stopping."
        exit "$code"
    fi
done
