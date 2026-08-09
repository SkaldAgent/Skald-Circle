use std::process::Stdio;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::tools::{
    SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
    truncate_label, MAX_LABEL_SHORT, MAX_LABEL_FULL,
};

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS:     u64 = 600;
const MAX_OUTPUT_BYTES:     usize = 100_000;

/// Returned by the context-free `Tool` entry points (`execute`/`execute_async`).
/// `execute_cmd` only ever runs through `run_with`, which carries the caller's
/// `ToolContext` and dispatches into the per-user container. There is no safe
/// host fallback (blueprint §6): running on the host would execute the command
/// in the Skald process itself, outside the sandbox the user approved.
const HOST_PATH_ERROR: &str =
    "execute_cmd requires the per-user container (ToolContext); it cannot run on the host";

pub struct ExecuteCmd;

impl Tool for ExecuteCmd {
    fn name(&self) -> &str { crate::tools::tool_names::EXECUTE_CMD }
    fn display_name(&self) -> &str { "Run Command" }
    fn icon(&self) -> &str { "shell" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Shell }

    fn description(&self) -> &str {
        // No capability advertisement here: which commands the sandbox has is the
        // system prompt's `<!-- SANDBOX_COMMANDS -->` section, which appears
        // exactly when this tool does. This description's job is the opposite one
        // — steering the model *away* from the shell for work a file tool does
        // better — and the two messages dilute each other.
        "Execute a shell command (sh -c) inside your sandbox container. \
         Reserve this for: builds, installs, git, tests, scripts, processes, network, package managers. \
         Runs as a non-root user; prefix system-package or global installs with `sudo` (e.g. `sudo apt-get install …`). \
         Do NOT use cat/head/tail to read files — use read_file instead. \
         Do NOT use grep/rg/find to search — use grep_files instead. \
         Do NOT use ls to list directories — use list_files instead. \
         Do NOT use sed/awk to edit files — use edit_file instead. \
         Do NOT use echo/cat heredoc to write files — use write_file instead. \
         Captures stdout and stderr. Requires user approval before running."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type":        "string",
                    "description": "Full command line, passed to `sh -c`. May include pipes, redirects, and shell expansions."
                },
                "workdir": {
                    "type":        "string",
                    "description": "Working directory for the command (an agent path like `projects/{owner}/{slug}` or `~`). \
                                    Omit to use your home directory (`~`)."
                },
                "timeout": {
                    "type":        "integer",
                    "description": format!(
                        "Max seconds to wait (default: {DEFAULT_TIMEOUT_SECS}, max: {MAX_TIMEOUT_SECS}). \
                         The command returns immediately when it finishes — set high for long builds, \
                         you won't wait unnecessarily."
                    ),
                    "default":     DEFAULT_TIMEOUT_SECS,
                    "minimum":     1,
                    "maximum":     MAX_TIMEOUT_SECS
                }
            },
            "required": ["command"]
        })
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let cmd = args["command"].as_str().unwrap_or("?");
        match length {
            ToolDescriptionLength::Short => {
                let binary = cmd.split_whitespace().next().unwrap_or(cmd);
                let name = binary.split('/').last().unwrap_or(binary);
                truncate_label(&format!("execute_cmd `{name}`"), MAX_LABEL_SHORT)
            }
            ToolDescriptionLength::Full => {
                truncate_label(&format!("execute_cmd `{cmd}`"), MAX_LABEL_FULL)
            }
        }
    }

    /// Context-free entry point — deliberately unreachable for real work. Without a
    /// `ToolContext` there is no per-user container to target, so this must NOT fall
    /// back to a host `sh -c` (blueprint §6 sandbox). Any dispatch that lands here
    /// (e.g. a REST resolve that bypasses the tool loop) is a caller bug: fail loud
    /// rather than escape the sandbox. The live path is `run_with`.
    fn execute(&self, _args: Value) -> Result<String> {
        anyhow::bail!(HOST_PATH_ERROR)
    }

    /// See [`Self::execute`]: no container without a `ToolContext`, so no host fallback.
    fn execute_async<'a>(&'a self, _args: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move { anyhow::bail!(HOST_PATH_ERROR) })
    }

    /// The real entry point (blueprint §6): the command runs **inside the caller's
    /// container** via `docker exec`, never on the host. `workdir` is interpreted as
    /// a path in the agent's namespace (`~/…`, `shared/{X}/…`) and mapped to its
    /// container path; omitted → the container home. Cancellation still works —
    /// `kill_on_drop` kills the `docker exec` client when the work future is dropped
    /// on /stop (best-effort; the in-container process may outlive it — see below).
    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        let container = ctx.fs.container_name.clone();
        let workdir = match args.get("workdir").and_then(Value::as_str) {
            Some(p) => ctx.fs.to_container(p),
            None    => ctx.fs.container_home.clone(),
        };
        let command = match args.get("command").and_then(Value::as_str) {
            Some(c) => c.to_string(),
            None    => return crate::tools::fs::error_exec("Missing required argument: command".to_string()),
        };
        let timeout_secs = args.get("timeout").and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        // Robust /stop: run the command in its own session/process-group whose
        // leader pid is recorded in a container-side pidfile. On /stop (the work
        // future dropped) or on a timeout, the `KillReaper` drop-guard reaps that
        // group via a second `docker exec … kill` — `kill_on_drop` alone only kills
        // the local `docker exec` client, not the tree Docker started *inside* the
        // container.
        let pidfile = format!("/tmp/skald-exec-{}.pgid", uuid::Uuid::new_v4());
        let wrapper = format!("echo $$ > {pidfile}; trap 'rm -f {pidfile}' EXIT; {command}");

        Box::new(SimpleExecution::new(Box::pin(async move {
            let guard = KillReaper::new(container.clone(), pidfile.clone());
            let out = run_in_container(&container, &workdir, &wrapper, &command, timeout_secs).await?;
            guard.disarm();
            Ok(ToolResult::Text(out))
        })))
    }
}

/// Runs a wrapped command inside a user's container:
/// `docker exec -w <wd> <container> setsid -w sh -c <script>`. Shares the
/// capture/timeout machinery with the host path; `label` is the original user
/// command, used only for logging and the timeout message.
///
/// `setsid -w` runs the command in its own session/process-group and propagates its
/// exit status; the caller's wrapper records the group-leader pid in a pidfile so a
/// [`KillReaper`] can `docker exec … kill` the whole group on /stop or timeout.
/// `kill_on_drop(true)` still tears down the local `docker exec` client at once, but
/// Docker does not propagate that to the in-container tree — which is why the reaper
/// exists.
async fn run_in_container(
    container:    &str,
    workdir:      &std::path::Path,
    script:       &str,
    label:        &str,
    timeout_secs: u64,
) -> Result<String> {
    tracing::info!(
        container = %container,
        workdir = %workdir.display(),
        command = %label,
        timeout_secs,
        "execute_cmd: running command in container"
    );

    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("exec")
        .arg("-w").arg(workdir)
        .arg(container)
        .arg("setsid").arg("-w")
        .arg("sh").arg("-c").arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    capture(cmd, timeout_secs, label).await
}

/// Drop-guard that reaps the in-container process group of an `execute_cmd` when the
/// work future is dropped before completing — i.e. on /stop, or after `run_in_container`
/// returns a timeout/spawn error (the `?` early-returns while the guard is still armed).
/// Disarmed on a clean exit, where the group is already gone. Best-effort: `Drop` spawns
/// a detached `docker exec … kill`; if no tokio runtime is current (shutdown) it is skipped.
struct KillReaper {
    container: String,
    pidfile:   String,
    armed:     bool,
}

impl KillReaper {
    fn new(container: String, pidfile: String) -> Self {
        Self { container, pidfile, armed: true }
    }

    /// The command completed on its own — nothing left to reap.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for KillReaper {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let container = self.container.clone();
        let pidfile   = self.pidfile.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move { reap_container_group(&container, &pidfile).await });
        }
    }
}

/// Reaper script: kills every process whose process-group equals the leader pid stored
/// in the pidfile (passed as `$1`), TERM then KILL after a grace, then removes the
/// pidfile. It walks `/proc` and signals members by **positive pid** rather than
/// `kill -<pgid>` because the container's `sh` (dash) mishandles a negative pgid
/// argument. The pidfile is a positional arg (`$1`), not string-interpolated, so an
/// arbitrary path is injection-safe and the script needs no brace-escaping. Killed
/// children are reaped by the container's `--init` (tini); without it they would linger
/// as harmless zombies.
const REAP_SCRIPT: &str = r#"
P=$(cat "$1" 2>/dev/null)
if [ -z "$P" ]; then rm -f "$1"; exit 0; fi
kids=""; ldr=""
for d in /proc/[0-9]*; do
  pid=$(basename "$d")
  st=$(cat "$d/stat" 2>/dev/null) || continue
  pg=$(printf "%s" "$st" | sed "s/.*) //" | cut -d" " -f3)
  if [ "$pg" = "$P" ]; then
    if [ "$pid" = "$P" ]; then ldr=$pid; else kids="$kids $pid"; fi
  fi
done
for pid in $kids $ldr; do kill -TERM "$pid" 2>/dev/null; done
sleep 2
for pid in $kids $ldr; do kill -KILL "$pid" 2>/dev/null; done
rm -f "$1"
"#;

/// Kills the process group recorded in `pidfile` inside `container` (see [`REAP_SCRIPT`])
/// and removes the pidfile. Runs as the container's user — the same uid that owns the
/// group — so no privilege is needed. A dead or absent group is a harmless no-op.
async fn reap_container_group(container: &str, pidfile: &str) {
    let _ = tokio::process::Command::new("docker")
        .arg("exec").arg(container)
        .arg("sh").arg("-c").arg(REAP_SCRIPT).arg("skald-reap").arg(pidfile)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

/// Spawns a prepared command, capturing stdout+stderr under a single timeout, and
/// formats the result. Used by the `docker exec` path (`run_in_container`).
async fn capture(mut cmd: tokio::process::Command, timeout_secs: u64, command: &str) -> Result<String> {
    let mut child = cmd.spawn()?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    // Read stdout/stderr concurrently with wait() inside a single timeout.
    // Reading after wait() deadlocks when the pipe buffer fills (~64KB).
    // The timeout must also cover the reads — background processes spawned by
    // the command can hold pipe descriptors open indefinitely after sh exits.
    let result = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        let (out_res, err_res, status_res) = tokio::join!(
            async {
                let mut buf = String::new();
                tokio::io::BufReader::new(stdout).read_to_string(&mut buf).await?;
                Ok::<_, std::io::Error>(buf)
            },
            async {
                let mut buf = String::new();
                tokio::io::BufReader::new(stderr).read_to_string(&mut buf).await?;
                Ok::<_, std::io::Error>(buf)
            },
            child.wait(),
        );
        Ok::<_, anyhow::Error>((out_res?, err_res?, status_res?))
    })
    .await;

    match result {
        Ok(Ok((out, err, status))) => {
            let code = status.code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let combined = format!("exit: {code}\n--- stdout ---\n{out}\n--- stderr ---\n{err}");
            Ok(truncate_output(combined))
        }
        Ok(Err(e)) => Err(e),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            anyhow::bail!("Command timed out after {timeout_secs}s: {command}");
        }
    }
}

fn truncate_output(s: String) -> String {
    if s.len() <= MAX_OUTPUT_BYTES {
        return s;
    }
    let head_size = MAX_OUTPUT_BYTES * 40 / 100;
    let tail_size = MAX_OUTPUT_BYTES - head_size;
    let head_end  = floor_char_boundary(&s, head_size);
    let tail_start = floor_char_boundary(&s, s.len().saturating_sub(tail_size));
    format!(
        "{}\n\n[... {} bytes omitted (showing first 40% and last 60%) ...]\n\n{}",
        &s[..head_end],
        s.len().saturating_sub(MAX_OUTPUT_BYTES),
        &s[tail_start..]
    )
}

fn floor_char_boundary(s: &str, idx: usize) -> usize {
    let mut i = idx.min(s.len());
    while !s.is_char_boundary(i) { i -= 1; }
    i
}
