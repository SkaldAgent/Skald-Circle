#!/usr/bin/env sh
# Supervisor loop for Skald.
#
# Runs a pre-built binary — it never compiles. Build with ./build.sh first:
#
#     ./build.sh && ./run.sh
#
# App exit codes:
#   0    graceful shutdown (SIGINT/SIGTERM) → stop the loop
#   255  restart requested → re-exec the same binary by path
#   *    error → propagate and stop
#
# The loop re-executes the binary *by path*, so after ./build.sh (atomic rename)
# a re-exec loads the new build. NOTE: the in-app `restart` tool was removed, so
# nothing currently produces 255 — this branch is kept for a future admin-only
# restart action. Today, restart manually: stop the server and re-run ./run.sh.

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
# If Python is not installed, the app starts normally but the TTS plugins (which
# spawn `python3` directly) will fail to start, and a host-run global connector
# will have no interpreter to install its own deps with. Per-user connectors are
# unaffected: they run inside the user's container, which ships its own Python.
VENV_DIR=".venv"
REQUIREMENTS="requirements.txt"

# A venv is usable only if python3 AND pip both work. Ubuntu's `python3 -m venv`
# without the python3-venv package leaves a venv with python3 but no pip — detect
# that and recreate, so a broken venv never survives a restart.
if [ ! -f "$VENV_DIR/bin/python3" ] || ! "$VENV_DIR/bin/python3" -m pip --version >/dev/null 2>&1; then
    rm -rf "$VENV_DIR"
    if command -v uv >/dev/null 2>&1; then
        echo "[run.sh] Setting up Python venv with uv …"
        uv venv --seed "$VENV_DIR" && uv pip install -r "$REQUIREMENTS" \
            && echo "[run.sh] Python venv ready." \
            || echo "[run.sh] Warning: Python venv setup failed — the TTS plugins and host-run connectors will be unavailable."
    elif command -v python3 >/dev/null 2>&1; then
        echo "[run.sh] Setting up Python venv …"
        python3 -m venv "$VENV_DIR" && "$VENV_DIR/bin/pip" install -r "$REQUIREMENTS" \
            && echo "[run.sh] Python venv ready." \
            || echo "[run.sh] Warning: Python venv setup failed — the TTS plugins and host-run connectors will be unavailable."
    else
        echo "[run.sh] Warning: python3 not found — the TTS plugins and host-run connectors will be unavailable."
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
