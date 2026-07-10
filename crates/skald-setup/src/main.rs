//! `skald-setup` — guided first-run setup.
//!
//! A terminal shell over `skald-core`: it opens the system database, and if no
//! user exists yet, walks whoever is at the keyboard through creating the first
//! admin. The real provisioning — envelope encryption, writing the file before
//! the row, rolling back on failure — lives in [`UserManager::register_user`];
//! this file is only the conversation around it.
//!
//! Run automatically by `run.sh` before the server loop. It is safe to run by
//! hand at any time: every step checks whether it is already satisfied and skips
//! itself, so re-running never duplicates anything.
//!
//! ## When it prompts
//!
//! Only when there is a real person to answer: no users yet **and** stdin is a
//! terminal. Headless (systemd, a container, CI) it prints one line and exits 0
//! so the server still starts — necessary while nothing consumes `users` yet and
//! the server runs fine with none. `SKALD_SKIP_SETUP=1` forces that path even on
//! a terminal.
//!
//! `--check` reports readiness without prompting: exit 0 if an admin exists,
//! exit 1 if setup is still needed. Anything else is a real error (exit 2).

use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result};
use skald_core::db::{self, SYSTEM_DB_PATH};
use skald_core::users::UserManager;

/// The role id given to the first user. There is no `roles` table yet, and
/// `users.role_id` has no foreign key, so this is a plain string for now — the
/// seeded `admin` preset (blueprint §12) will adopt it later without a migration.
const ADMIN_ROLE: &str = "admin";

/// SQLCipher's per-user privacy is only as strong as the password's entropy
/// times the KDF cost (§5.1). The KDF is fixed; this is the floor we put under
/// the entropy. Not a substitute for a real strength meter — a deliberate,
/// visible minimum.
const MIN_PASSWORD_LEN: usize = 10;

fn main() -> std::process::ExitCode {
    let mode = match parse_args() {
        Ok(m) => m,
        Err(msg) => {
            eprintln!("{msg}");
            return std::process::ExitCode::from(2);
        }
    };

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("skald-setup: failed to start async runtime: {e}");
            return std::process::ExitCode::from(2);
        }
    };

    match rt.block_on(run(mode)) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("skald-setup: {e:#}");
            std::process::ExitCode::from(2)
        }
    }
}

enum Mode {
    /// Prompt if needed, otherwise a no-op.
    Run,
    /// Report readiness via exit code, never prompt.
    Check,
}

fn parse_args() -> Result<Mode, String> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None => Ok(Mode::Run),
        Some("--check") => Ok(Mode::Check),
        Some("--help" | "-h") => Err(usage()),
        Some(other) => Err(format!("skald-setup: unknown argument {other:?}\n\n{}", usage())),
    }
}

fn usage() -> String {
    "skald-setup — guided first-run setup\n\n\
     usage:\n  \
       skald-setup            create the first admin if none exists (interactive)\n  \
       skald-setup --check    exit 0 if setup is done, 1 if still needed\n\n\
     environment:\n  \
       SKALD_SKIP_SETUP=1     never prompt, even on a terminal"
        .to_string()
}

async fn run(mode: Mode) -> Result<std::process::ExitCode> {
    // Opening the pool creates `database/system.db` and its schema if absent —
    // the same call the server makes, so setup and server agree on the layout.
    let pool = db::init_system_pool(SYSTEM_DB_PATH)
        .await
        .context("opening the system database")?;
    let users = UserManager::new(std::sync::Arc::new(pool));

    let has_admin = users.count().await.context("counting users")? > 0;

    if let Mode::Check = mode {
        return Ok(if has_admin {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::FAILURE
        });
    }

    // ── Steps ──────────────────────────────────────────────────────────────
    // Each is idempotent: it decides for itself whether there is work to do.
    // Today there is one. Provider and model setup will be added here as further
    // steps, in order, each skipping itself when already configured.
    step_first_user(&users, has_admin).await?;

    Ok(std::process::ExitCode::SUCCESS)
}

/// Create the first admin, or do nothing if one already exists.
async fn step_first_user(users: &UserManager, has_admin: bool) -> Result<()> {
    if has_admin {
        // Idempotent re-run, or a second binary got there first.
        return Ok(());
    }

    // No one to answer ⇒ don't block the server. It runs fine with no users
    // today, so a missing admin is a note, not a failure.
    if std::env::var_os("SKALD_SKIP_SETUP").is_some() || !io::stdin().is_terminal() {
        eprintln!(
            "skald-setup: no users configured. Run `./bin/skald-setup` from a terminal \
             to create the first admin."
        );
        return Ok(());
    }

    println!("\nWelcome to Skald. Let's create the first user (the admin).\n");

    let username = prompt_username()?;
    let display_name = prompt_line("Display name (optional): ")?;
    let display_name = display_name.trim();
    let display_name = (!display_name.is_empty()).then_some(display_name);

    let encrypt = prompt_encrypt()?;
    let password = prompt_new_password()?;

    let id = users
        .register_user(&username, display_name, ADMIN_ROLE, Some(&password), encrypt)
        .await
        .context("creating the admin user")?;

    println!("\n✓ Admin user '{username}' created (id {id}).");
    if encrypt {
        println!("  Their private database is encrypted. There is no recovery if the password is lost.");
    }
    println!();
    Ok(())
}

// ── Prompts ────────────────────────────────────────────────────────────────

fn prompt_username() -> Result<String> {
    loop {
        let name = prompt_line("Username: ")?;
        let name = name.trim();
        if name.is_empty() {
            println!("  A username is required.");
            continue;
        }
        // The username is a login handle; keep it to something a person types.
        if name.contains(char::is_whitespace) {
            println!("  No spaces in a username.");
            continue;
        }
        return Ok(name.to_string());
    }
}

/// Default yes, with the honest caveat shown before the choice. For the admin —
/// who owns the box — encryption guards against a stolen machine, not against
/// the other users (§2/§4); and it has no recovery. The prompt says so.
fn prompt_encrypt() -> Result<bool> {
    println!("Encrypt this user's private conversations at rest?");
    println!("  • Only this user, unlocked with their password, can read them.");
    println!("  • There is NO recovery: if the password is lost, the data is gone.");
    loop {
        let ans = prompt_line("Encrypt? [Y/n]: ")?;
        match ans.trim().to_lowercase().as_str() {
            "" | "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("  Please answer y or n."),
        }
    }
}

/// Reads a password twice, without echo, and holds it to a visible floor.
fn prompt_new_password() -> Result<String> {
    loop {
        let pw = rpassword::prompt_password("Password: ").context("reading password")?;
        if let Some(reason) = weak_password(&pw) {
            println!("  {reason}");
            continue;
        }
        let again = rpassword::prompt_password("Confirm password: ").context("reading password")?;
        if pw != again {
            println!("  The passwords did not match — try again.");
            continue;
        }
        return Ok(pw);
    }
}

/// A blunt weak-password check: a length floor plus the handful of passwords an
/// attacker guesses first. Not a strength meter — enough to stop the obviously
/// hopeless choice on an account whose password *is* the encryption key.
fn weak_password(pw: &str) -> Option<String> {
    if pw.chars().count() < MIN_PASSWORD_LEN {
        return Some(format!("Too short — use at least {MIN_PASSWORD_LEN} characters."));
    }
    const COMMON: &[&str] = &[
        "password", "password1", "1234567890", "12345678", "qwertyuiop",
        "letmein", "iloveyou", "admin", "welcome", "changeme", "skald",
    ];
    if COMMON.contains(&pw.to_lowercase().as_str()) {
        return Some("That is one of the most common passwords — choose another.".to_string());
    }
    None
}

fn prompt_line(label: &str) -> Result<String> {
    print!("{label}");
    io::stdout().flush().ok();
    let mut line = String::new();
    let n = io::stdin().read_line(&mut line).context("reading input")?;
    if n == 0 {
        // EOF on a terminal (Ctrl-D): the person is declining. Stop the whole
        // supervisor rather than looping on empty input.
        anyhow::bail!("setup cancelled");
    }
    Ok(line)
}
