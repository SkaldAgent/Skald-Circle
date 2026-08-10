//! Verify-before-save for MCP connectors (blueprint §15 verify step).
//!
//! When a user fills the activation form, Skald can run the connector's declared
//! `verify` command to confirm the credentials actually work *before* persisting
//! the activation. This module owns:
//!
//! - [`apply_placeholders`] — the single substitution engine for `{ENV:NAME}`
//!   and `{SECRET:NAME}` tokens (used here for the verify command, and by the
//!   MCP transport for URLs / env values).
//! - [`run_verify`] — launches the resolved command either on the host (for a
//!   global `mcp_remote` connector) or inside the caller's container (for a
//!   per-user `mcp_local` connector), parses the JSON result, and returns a
//!   [`VerifyReport`].
//!
//! Output contract: the verify command must print one JSON object on stdout,
//! `{"ok": bool, "message": string, "details"?: object}`, and exit 0 on success.
//! If the JSON parse fails, [`run_verify`] falls back to the exit code. Secrets
//! are never logged.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tracing::debug;

/// Default timeout for a verify command (seconds). Overridable per-connector via
/// the manifest's `verify.timeout_secs`.
pub const DEFAULT_VERIFY_TIMEOUT_SECS: u64 = 15;

/// The outcome of a verify run, surfaced to the UI verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    /// `true` when the credentials check out.
    pub ok: bool,
    /// Human-readable result line (shown next to the Test button).
    pub message: String,
    /// Optional structured details (shown in a `<pre>` block). Never holds
    /// secrets — the verify script is responsible for not echoing them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Wall-clock time the command took.
    #[serde(skip)]
    pub elapsed: Duration,
    /// `true` when the connector declares no `verify` step, so "no test" must
    /// be distinguishable from "test passed" in the UI.
    #[serde(skip)]
    pub skipped: bool,
}

impl VerifyReport {
    /// Synthesized when the connector has no `verify` step — the UI shows
    /// "no test available" rather than a pass/fail.
    pub fn skipped() -> Self {
        Self {
            ok: true,
            message: "This connector has no verification step.".into(),
            details: None,
            elapsed: Duration::ZERO,
            skipped: true,
        }
    }
}

/// Where [`run_verify`] executes the command. Mirrors `McpServerSpec.launch_in`:
/// `None` runs on the host (a global `mcp_remote` connector), `Some(container)`
/// runs inside the user's container via `docker exec`.
pub enum VerifyTarget<'a> {
    /// Run on the Skald host process. `workdir` is an absolute host path
    /// (typically `connectors/<folder>/`).
    Host { workdir: &'a Path },
    /// Run inside the user's sandbox container. `workdir` is an absolute path
    /// *inside* the container (e.g. `/root/.skald/mcp/<name>`).
    Container {
        container: &'a str,
        workdir: &'a Path,
    },
}

/// Substitutes `{ENV:NAME}` and `{SECRET:NAME}` tokens in `text`.
///
/// - `{ENV:NAME}` → `env[NAME]`, or empty string if absent.
/// - `{SECRET:NAME}` → `secret[NAME]`, or empty string if absent.
/// - Any other `{...}` token is left untouched — `{key}` belongs to the remote
///   transport's URL substitution (see `mcp::apply_key`), and anything else is a
///   misconfiguration that should stay visible rather than be silently erased.
pub fn apply_placeholders(
    text: &str,
    env: &HashMap<String, String>,
    secret: &HashMap<String, String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        // Append everything up to the '{'.
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        if let Some(close) = after.find('}') {
            let token = &after[..=close]; // includes both braces
            let inner = token.strip_prefix('{').unwrap().strip_suffix('}').unwrap();
            // ENV:/SECRET: tokens are always consumed (missing key → empty);
            // any other `{...}` is left untouched so a misconfiguration stays
            // visible rather than being silently erased.
            if let Some(name) = inner.strip_prefix("ENV:") {
                out.push_str(env.get(name).map(|s| s.as_str()).unwrap_or(""));
            } else if let Some(name) = inner.strip_prefix("SECRET:") {
                out.push_str(secret.get(name).map(|s| s.as_str()).unwrap_or(""));
            } else {
                out.push_str(token);
            }
            rest = &after[close + 1..];
        } else {
            // No closing brace — emit the rest literally and stop.
            out.push_str(after);
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Runs the verify `command` (after placeholder substitution) in the given
/// target, injects the env/secret values as environment variables, captures
/// stdout/stderr under a timeout, and parses the JSON result.
///
/// The command and resolved env are NOT logged (secrets may be inline). Only
/// the final `ok`/`message` are traced at debug level.
pub async fn run_verify(
    command: &str,
    env_values: &HashMap<String, String>,
    secret_values: &HashMap<String, String>,
    target: VerifyTarget<'_>,
    timeout_secs: u64,
) -> VerifyReport {
    let resolved = apply_placeholders(command, env_values, secret_values);
    let timeout = Duration::from_secs(timeout_secs.max(1));
    let started = Instant::now();

    // Build the process: `docker exec … sh -c "<cmd>"` or host `sh -c "<cmd>"`.
    let mut cmd = build_command(&target, &resolved, env_values, secret_values);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return VerifyReport {
                ok: false,
                message: format!("Could not start the verify command: {e}"),
                details: None,
                elapsed: started.elapsed(),
                skipped: false,
            };
        }
    };

    let outcome = run_with_timeout(child, timeout).await;
    let elapsed = started.elapsed();

    let report = parse_verify_output(&outcome, elapsed);
    debug!(ok = report.ok, elapsed_ms = elapsed.as_millis() as u64, "verify");
    report
}

/// Collects the child's stdout/stderr under a single timeout, returning the
/// captured buffers and the exit code (None if killed by timeout).
async fn run_with_timeout(
    mut child: tokio::process::Child,
    timeout: Duration,
) -> VerifyOutcome {
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");

    let collect = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        // Read concurrently — the pipes are independent.
        let r1 = stdout.read_to_end(&mut out);
        let r2 = stderr.read_to_end(&mut err);
        let (ro, re, status) = tokio::join!(r1, r2, child.wait());
        ro.map_err(anyhow::Error::from)?;
        re.map_err(anyhow::Error::from)?;
        let code = status.ok().and_then(|s| s.code());
        Ok::<_, anyhow::Error>((out, err, code))
    };

    match tokio::time::timeout(timeout, collect).await {
        Ok(Ok((out, err, code))) => VerifyOutcome { stdout: out, stderr: err, code, timed_out: false },
        // Inner error (spawn/io).
        Ok(Err(e)) => VerifyOutcome {
            stdout: Vec::new(),
            stderr: e.to_string().into_bytes(),
            code: None,
            timed_out: false,
        },
        // Timeout: kill_on_drop takes care of the child.
        Err(_) => VerifyOutcome {
            stdout: Vec::new(),
            stderr: format!("verify timed out after {}s", timeout.as_secs()).into_bytes(),
            code: None,
            timed_out: true,
        },
    }
}

struct VerifyOutcome {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
    timed_out: bool,
}

/// Parses the verify command's output into a [`VerifyReport`].
///
/// Contract: the command prints one JSON object on stdout:
/// `{"ok": bool, "message": string, "details"?: object}`. If the parse fails,
/// falls back to the exit code (0 = ok, anything else = fail) and uses stderr
/// (or stdout) as the message.
fn parse_verify_output(outcome: &VerifyOutcome, elapsed: Duration) -> VerifyReport {
    let stdout = String::from_utf8_lossy(&outcome.stdout);
    let stderr = String::from_utf8_lossy(&outcome.stderr);

    if outcome.timed_out {
        return VerifyReport {
            ok: false,
            message: stderr.trim().to_string(),
            details: None,
            elapsed,
            skipped: false,
        };
    }

    // Try JSON parse first (prefer the last line, in case the script emitted a
    // trailing newline or a preamble).
    let trimmed = stdout.trim();
    if !trimmed.is_empty() {
        if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
            let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or_else(|| outcome.code == Some(0));
            let message = v
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            let details = v.get("details").cloned();
            return VerifyReport { ok, message, details, elapsed, skipped: false };
        }
    }

    // Fallback: exit-code semantics. Empty stdout → fall back to stderr.
    let ok = outcome.code == Some(0);
    let message = if !trimmed.is_empty() {
        trimmed.to_string()
    } else if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else if ok {
        "Verification succeeded.".into()
    } else {
        format!("Verify failed (exit code {}).", outcome.code.unwrap_or(-1))
    };
    VerifyReport { ok, message, details: None, elapsed, skipped: false }
}

/// Builds the verify process for the given target.
///
/// `docker exec` syntax is `docker exec [OPTIONS] CONTAINER COMMAND [ARG...]`:
/// every argument after the container name is the COMMAND, so the `-e` env
/// flags must come BEFORE the container name — placing them after makes docker
/// try to execute a binary named `-e` ("exec: \"-e\": executable file not
/// found"). The MCP server launch (`mcp-client/src/server.rs`) already builds
/// it in this order; keep the two in sync.
fn build_command(
    target: &VerifyTarget<'_>,
    resolved: &str,
    env_values: &HashMap<String, String>,
    secret_values: &HashMap<String, String>,
) -> tokio::process::Command {
    match target {
        VerifyTarget::Container { container, workdir } => {
            let vars = verify_env(workdir, env_values, secret_values);
            let mut c = tokio::process::Command::new("docker");
            c.arg("exec").arg("-w").arg(workdir);
            inject_env_flags(&mut c, &vars);
            c.arg(container);
            c.arg("sh").arg("-c").arg(resolved);
            c
        }
        VerifyTarget::Host { workdir } => {
            let vars = verify_env(workdir, env_values, secret_values);
            let mut c = tokio::process::Command::new("sh");
            c.arg("-c").arg(resolved).current_dir(workdir);
            inject_env_vars(&mut c, &vars);
            c
        }
    }
}

/// The full environment for a verify run: the form's env + secret values, plus a
/// derived `PYTHONPATH` pointing at the connector's own `.pydeps`.
///
/// Without that last part a well-written python connector is **rejected by its own
/// verify**. Its dependencies are installed under `<connector-dir>/.pydeps`
/// ([`install::ensure_installed`] / [`install::ensure_installed_host`]) and only the
/// *server* launch ever put them on `PYTHONPATH` (`mcp::global_row_spec` /
/// `user_row_spec`); the verify runs as a bare `sh -c` and inherits nothing. In
/// `global_enable` the install runs *before* the verify, so the deps are sitting
/// installed in the very directory the verify then declares them missing from — and
/// the row ends up `enabled = 0`. Only connectors that bother to declare a `verify`
/// hit it.
///
/// The workdir *is* the connector dir in both targets (`global_verify_workdir` and
/// `prepare_user_verify_workdir`), so the path needs no new parameter. Node needs no
/// equivalent: `node_modules/` beside the entry file resolves from the cwd, which is
/// that same workdir.
///
/// Set only when the form did not declare one — `or_insert`, not `insert`, mirroring
/// `global_row_spec`: an explicit `PYTHONPATH` is the connector author's call. Adding
/// it unconditionally is harmless for a node or remote connector, since nothing there
/// reads it.
fn verify_env(
    workdir: &Path,
    env: &HashMap<String, String>,
    secret: &HashMap<String, String>,
) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = env
        .iter()
        .chain(secret.iter())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if !vars.iter().any(|(k, _)| k == PYTHONPATH_VAR) {
        let pydeps = workdir.join(super::install::PYDEPS_DIR);
        vars.push((PYTHONPATH_VAR.to_string(), pydeps.to_string_lossy().into_owned()));
    }
    vars
}

/// The variable [`verify_env`] derives. Named so the "don't override the form's own
/// value" check and the value it would set cannot drift apart.
const PYTHONPATH_VAR: &str = "PYTHONPATH";

/// Adds `-e KEY=VALUE` flags for `docker exec`.
fn inject_env_flags(cmd: &mut tokio::process::Command, vars: &[(String, String)]) {
    for (k, v) in vars {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
}

/// Sets environment variables for a host `sh -c` process.
fn inject_env_vars(cmd: &mut tokio::process::Command, vars: &[(String, String)]) {
    for (k, v) in vars {
        cmd.env(k, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(items: &[(&str, &str)]) -> HashMap<String, String> {
        items.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn placeholders_env_and_secret() {
        let env = m(&[("HOST", "imap.example.com"), ("PORT", "993")]);
        let secret = m(&[("PASS", "hunter2")]);
        let s = apply_placeholders("h={ENV:HOST} p={ENV:PORT} s={SECRET:PASS}", &env, &secret);
        assert_eq!(s, "h=imap.example.com p=993 s=hunter2");
    }

    #[test]
    fn placeholders_missing_become_empty() {
        let env = m(&[("HOST", "x")]);
        let secret = HashMap::new();
        let s = apply_placeholders("[{ENV:HOST}][{ENV:MISSING}][{SECRET:X}]", &env, &secret);
        assert_eq!(s, "[x][][]");
    }

    #[test]
    fn placeholders_unknown_left_untouched() {
        let env = HashMap::new();
        let secret = HashMap::new();
        let s = apply_placeholders("{key} {ENV:A} {0}", &env, &secret);
        assert_eq!(s, "{key}  {0}");
    }

    #[test]
    fn placeholders_no_braces() {
        let env = HashMap::new();
        let secret = HashMap::new();
        assert_eq!(apply_placeholders("plain text", &env, &secret), "plain text");
    }

    #[test]
    fn placeholders_unclosed_brace_kept() {
        let env = HashMap::new();
        let secret = HashMap::new();
        assert_eq!(apply_placeholders("a {ENV:B c", &env, &secret), "a {ENV:B c");
    }

    #[test]
    fn container_command_places_env_flags_before_container_name() {
        let env = m(&[("HOST", "imap.example.com")]);
        let secret = m(&[("PASS", "hunter2")]);
        let target = VerifyTarget::Container {
            container: "skald-user1",
            workdir: Path::new("/root/.skald/mcp/email"),
        };
        let cmd = build_command(&target, "python3 verify.py", &env, &secret);
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let container_pos = args.iter().position(|a| a == "skald-user1").unwrap();
        // Every `-e KEY=VALUE` pair must come before the container name:
        // after it, docker parses arguments as the COMMAND to run.
        for (i, a) in args.iter().enumerate() {
            if a == "-e" {
                assert!(i + 1 < container_pos, "-e flag at {i} is not before the container name: {args:?}");
                assert!(args[i + 1].contains('='), "-e must be followed by KEY=VALUE: {args:?}");
            }
        }
        assert_eq!(&args[..2], &["exec", "-w"]);
        assert_eq!(args[container_pos..], ["skald-user1", "sh", "-c", "python3 verify.py"]);
    }

    #[test]
    fn container_command_without_env_still_carries_pythonpath() {
        let target = VerifyTarget::Container {
            container: "skald-user1",
            workdir: Path::new("/root/.skald/mcp/x"),
        };
        let cmd = build_command(&target, "true", &HashMap::new(), &HashMap::new());
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "exec", "-w", "/root/.skald/mcp/x",
                "-e", "PYTHONPATH=/root/.skald/mcp/x/.pydeps",
                "skald-user1", "sh", "-c", "true",
            ]
        );
    }

    #[test]
    fn verify_env_derives_pythonpath_from_the_workdir() {
        let vars = verify_env(
            Path::new("/srv/skald/connectors/gmaps"),
            &m(&[("REGION", "eu")]),
            &m(&[("KEY", "abc")]),
        );
        let pp = vars.iter().find(|(k, _)| k == "PYTHONPATH").expect("PYTHONPATH derived");
        assert_eq!(pp.1, "/srv/skald/connectors/gmaps/.pydeps");
        // The form's own values are untouched.
        assert!(vars.iter().any(|(k, v)| k == "REGION" && v == "eu"));
        assert!(vars.iter().any(|(k, v)| k == "KEY" && v == "abc"));
    }

    #[test]
    fn verify_env_does_not_override_a_declared_pythonpath() {
        let vars = verify_env(
            Path::new("/srv/skald/connectors/gmaps"),
            &m(&[("PYTHONPATH", "/opt/vendored")]),
            &HashMap::new(),
        );
        let pps: Vec<&String> = vars.iter().filter(|(k, _)| k == "PYTHONPATH").map(|(_, v)| v).collect();
        assert_eq!(pps, ["/opt/vendored"], "the connector's own value must win, and only once");
    }

    #[test]
    fn host_command_runs_in_the_workdir_with_pythonpath() {
        let target = VerifyTarget::Host { workdir: Path::new("/srv/skald/connectors/gmaps") };
        let cmd = build_command(&target, "python3 verify.py", &HashMap::new(), &HashMap::new());
        let std = cmd.as_std();
        assert_eq!(std.get_current_dir(), Some(Path::new("/srv/skald/connectors/gmaps")));
        let pp = std
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("PYTHONPATH"))
            .and_then(|(_, v)| v)
            .expect("PYTHONPATH set");
        assert_eq!(pp, std::ffi::OsStr::new("/srv/skald/connectors/gmaps/.pydeps"));
    }

    #[test]
    fn parse_json_ok() {
        let o = VerifyOutcome {
            stdout: br#"{"ok": true, "message": "all good"}"#.to_vec(),
            stderr: vec![],
            code: Some(0),
            timed_out: false,
        };
        let r = parse_verify_output(&o, Duration::from_millis(10));
        assert!(r.ok);
        assert_eq!(r.message, "all good");
    }

    #[test]
    fn parse_json_fail_with_details() {
        let o = VerifyOutcome {
            stdout: br#"{"ok": false, "message": "bad creds", "details": {"imap": "ok", "smtp": "no"}}"#.to_vec(),
            stderr: vec![],
            code: Some(1),
            timed_out: false,
        };
        let r = parse_verify_output(&o, Duration::from_millis(10));
        assert!(!r.ok);
        assert_eq!(r.message, "bad creds");
        assert_eq!(r.details.unwrap()["smtp"], "no");
    }

    #[test]
    fn parse_fallback_exit_code() {
        let o = VerifyOutcome {
            stdout: b"some plain output".to_vec(),
            stderr: vec![],
            code: Some(0),
            timed_out: false,
        };
        let r = parse_verify_output(&o, Duration::from_millis(10));
        assert!(r.ok);
        assert_eq!(r.message, "some plain output");
    }

    #[test]
    fn parse_fallback_stderr_on_fail() {
        let o = VerifyOutcome {
            stdout: vec![],
            stderr: b"connection refused".to_vec(),
            code: Some(2),
            timed_out: false,
        };
        let r = parse_verify_output(&o, Duration::from_millis(10));
        assert!(!r.ok);
        assert_eq!(r.message, "connection refused");
    }

    #[test]
    fn parse_timeout_is_fail() {
        let o = VerifyOutcome {
            stdout: vec![],
            stderr: b"verify timed out after 15s".to_vec(),
            code: None,
            timed_out: true,
        };
        let r = parse_verify_output(&o, Duration::from_secs(15));
        assert!(!r.ok);
        assert!(r.message.contains("timed out"));
    }
}
