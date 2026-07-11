use std::path::PathBuf;
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

pub struct ExecuteCmd;

impl Tool for ExecuteCmd {
    fn name(&self) -> &str { crate::tools::tool_names::EXECUTE_CMD }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Shell }

    fn description(&self) -> &str {
        "Execute a shell command (sh -c) inside your sandbox container (python + node available). \
         Reserve this for: builds, installs, git, tests, scripts, processes, network, package managers. \
         Do NOT use cat/head/tail to read files — use read_file instead. \
         Do NOT use grep/rg/find to search — use grep_files instead. \
         Do NOT use ls to list directories — use list_files instead. \
         Do NOT use sed/awk to edit files — use edit_file instead. \
         Do NOT use echo/cat heredoc to write files — use write_file instead. \
         Captures stdout and stderr. Requires user approval before running."
    }

    fn parameters_schema(&self) -> Value {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string());

        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type":        "string",
                    "description": "Full command line, passed to `sh -c`. May include pipes, redirects, and shell expansions."
                },
                "workdir": {
                    "type":        "string",
                    "description": format!(
                        "Working directory for the command (absolute path). \
                         Omit to use the project root (currently: {cwd})."
                    )
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

    fn execute(&self, args: Value) -> Result<String> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(run_from_args(&args))
        })
    }

    /// Genuinely async so the unified `ToolExecution` path can race it against the
    /// /stop token: on cancel the `SimpleExecution` drops this future and
    /// `kill_on_drop(true)` kills the spawned shell process. (The sync `execute`
    /// above — which blocks a worker thread — would not be cancellable.)
    fn execute_async<'a>(&'a self, args: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move { run_from_args(&args).await })
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

        Box::new(SimpleExecution::new(Box::pin(async move {
            Ok(ToolResult::Text(run_in_container(&container, &workdir, &command, timeout_secs).await?))
        })))
    }
}

/// Runs a command inside a user's container: `docker exec -w <wd> <container> sh -c <cmd>`.
/// Shares the capture/timeout machinery with the host path.
///
/// ⚠️ Cancellation caveat: dropping the `docker exec` client on /stop kills that
/// client process, but Docker does not guarantee the process it started *inside*
/// the container dies with it. For long-running in-container work a robust stop
/// would track the PID and `docker exec … kill`; that is a follow-up.
async fn run_in_container(
    container:    &str,
    workdir:      &std::path::Path,
    command:      &str,
    timeout_secs: u64,
) -> Result<String> {
    tracing::info!(
        container = %container,
        workdir = %workdir.display(),
        command = %command,
        timeout_secs,
        "execute_cmd: running command in container"
    );

    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("exec")
        .arg("-w").arg(workdir)
        .arg(container)
        .arg("sh").arg("-c").arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    capture(cmd, timeout_secs, command).await
}

/// Parse + run a shell command from tool arguments, as an awaitable future.
///
/// Driven by `ExecuteCmd::execute_async` through the unified `ToolExecution`
/// path: on /stop the `SimpleExecution` drops this future and `kill_on_drop(true)`
/// kills the child process. `Tool::execute` runs it synchronously via
/// `block_in_place` only as a non-cancellable fallback.
pub async fn run_from_args(args: &Value) -> Result<String> {
    let (command, workdir, timeout_secs) = parse_args(args)?;
    run(command, workdir, timeout_secs).await
}

fn parse_args(args: &Value) -> Result<(String, Option<PathBuf>, u64)> {
    let command = args["command"].as_str()
        .ok_or_else(|| anyhow::anyhow!("Missing required argument: command"))?
        .to_string();

    let workdir = match args["workdir"].as_str() {
        Some(p) => {
            let path = PathBuf::from(p);
            if !path.is_absolute() {
                anyhow::bail!("workdir must be an absolute path, got: {p}");
            }
            if !path.is_dir() {
                anyhow::bail!("workdir does not exist or is not a directory: {p}");
            }
            Some(path)
        }
        None => None,
    };

    let timeout_secs = args["timeout"].as_u64()
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);

    Ok((command, workdir, timeout_secs))
}

async fn run(command: String, workdir: Option<PathBuf>, timeout_secs: u64) -> Result<String> {
    // Audit log: record every shell command before it runs. Auto-approved
    // commands (approval bypass active) otherwise leave no trace, so a command
    // that kills the process — or misbehaves — can't be reconstructed.
    let workdir_display = workdir
        .as_deref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".to_string());
    tracing::info!(
        command = %command,
        workdir = %workdir_display,
        timeout_secs,
        "execute_cmd: running shell command"
    );

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(&command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }

    capture(cmd, timeout_secs, &command).await
}

/// Spawns a prepared command, capturing stdout+stderr under a single timeout, and
/// formats the result. Shared by the host `sh -c` path and the `docker exec` path.
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
