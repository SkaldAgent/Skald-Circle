# Self-Rewriting

## Restart Mechanism

The `restart` tool has **two modes**, branched on the `desktop` cargo feature:

| Mode | Behaviour | Triggers |
|---|---|---|
| **Headless** (`cargo run`, `run.sh`, Docker) | `libc::_exit(-1)` → exit code `255`. `run.sh` detects `255` and re-executes `cargo run`, which recompiles changed source files and relaunches. The supervisor is the only thing that can rebuild the binary. | The agent edited `src/**/*.rs` or `Cargo.toml` and wants the new code loaded. |
| **Desktop** (`--features desktop`, Tauri bundle) | `AppHandle::cleanup_before_exit()` (Tauri-side teardown) → `Command::new(current_exe).spawn()` → `std::process::exit(0)`. The same read-only bundled binary is relaunched — **no rebuild** (there is no source tree to rebuild from). | The user/agent wants to apply `config.yml` / DB changes that are only read at startup. Self-modification of `src/**/*.rs` has no effect in a bundle. |

The headless exit code `255` (`-1`) deliberately uses `libc::_exit()` rather
than `std::process::exit()` to skip C `atexit` handlers (e.g. Metal GPU cleanup
in `whisper-rs`, which would crash with SIGABRT and produce `134` instead of
`255`).

See [desktop.md](desktop.md) for the desktop-bundle architecture and the
`desktop::app_handle()` OnceLock that lets the tool reach the `AppHandle`.

---

## run.sh Exit Codes

| Exit code | Meaning | run.sh action |
|---|---|---|
| `0` | Graceful shutdown — SIGINT (Ctrl+C) **or** SIGTERM, both trapped in `main.rs` | Stop loop, exit 0 |
| `255` | Restart requested (`exit(-1)`) | `cargo run` again (recompile) |
| `143` | SIGTERM with no handler (`128+15`) — no longer reachable; see note | Stop loop, propagate code |
| other | Unexpected error (e.g. `101` panic) | Stop loop, propagate code |

`main.rs` traps **both** SIGINT and SIGTERM (`wait_for_shutdown_signal`) and runs the graceful shutdown path, so an external `kill` now exits `0` and logs `signal=SIGTERM` instead of dying silently with code `143`. To force a restart, use the `restart` tool (exit `255`) — never `kill` the process.

> The `run.sh` supervisor and exit-code table above apply only to **headless
> mode**. In desktop mode there is no supervisor; the Tauri process manages its
> own lifecycle and exit code `0` simply terminates the app.

---

## Safe Self-Modification Workflow

1. **Read** the relevant source files with `read_file` before making any changes.
2. **Edit** source files (`edit_file`, `write_file`, etc.).
3. **Check**: `execute_cmd` with command `cargo check 2>&1`. Inspect output.
4. **Fix** any compiler errors. Repeat steps 2–3 until clean.
5. **Restart**: call the `restart` tool only after a clean `cargo check`. The app rebuilds and relaunches automatically.

Never skip the `cargo check` step. A broken build will crash the supervisor loop with a non-zero non-255 exit code, stopping the app entirely.

---

## Requires Restart vs Does Not

| Change | Restart required? |
|---|---|
| `src/**/*.rs` | **Yes** |
| `Cargo.toml` / `Cargo.lock` | **Yes** |
| `agents/*/AGENT.md` | No — read at request time |
| `agents/*/meta.json` | No — read at request time |
| `config.yml` | No — read at startup only; take effect on next restart |
| `data/memory/**` | No — read at request time |
| `docs/**` | No |

---

## Risk Points

- **Never call `restart` mid-approval flow.** If a `PendingWrite` is waiting for user input, calling `restart` drops the `oneshot` sender, which unblocks the handler with an `Err` — the approval is cancelled and the tool call is aborted. Wait for the approval to resolve first.
- **Always check build before restart.** A compilation failure with `cargo run` returns a non-255 exit code, causing `run.sh` to stop the loop rather than retry.
- **`execute_cmd` requires user approval.** The user must approve the shell command in the UI before it executes.

---

## When to Update This File

- The restart mechanism or exit codes change
- The safe-modification workflow gains or loses a step
- New file types are added that do/don't require a restart
