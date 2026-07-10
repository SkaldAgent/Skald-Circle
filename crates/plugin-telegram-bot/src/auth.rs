use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use super::TgShared;

// ── Whitelist file schema ─────────────────────────────────────────────────────
//
// Written to secrets/telegram_whitelist.json.
// The main agent edits this file directly to authorise users.

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct WhitelistFile {
    #[serde(default)]
    pub whitelist: Vec<i64>,
    #[serde(default)]
    pub pending_pairings: Vec<PairingEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PairingEntry {
    pub code:       String,
    pub chat_id:    i64,
    pub issued_at:  String,
}

pub(crate) async fn load_wl(secrets_dir: &Path) -> WhitelistFile {
    let path = secrets_dir.join("telegram_whitelist.json");
    match tokio::fs::read_to_string(&path).await {
        Ok(s)  => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => WhitelistFile::default(),
    }
}

pub(crate) async fn save_wl(secrets_dir: &Path, wl: &WhitelistFile) -> Result<()> {
    tokio::fs::create_dir_all(secrets_dir).await?;
    let path = secrets_dir.join("telegram_whitelist.json");
    tokio::fs::write(&path, serde_json::to_string_pretty(wl)?).await?;
    Ok(())
}

// ── Pairing ───────────────────────────────────────────────────────────────────

/// Pairing codes older than this are considered abandoned and pruned, so the
/// whitelist file does not accumulate stale `pending_pairings` entries.
const PAIRING_TTL_HOURS: i64 = 24;

pub(crate) async fn handle_pairing(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    let mut wl = load_wl(&shared.secrets_dir).await;

    // Drop pairing codes past their TTL. Entries with an unparseable timestamp
    // are kept (don't silently lose data on a format change).
    let cutoff = Utc::now() - chrono::Duration::hours(PAIRING_TTL_HOURS);
    let before = wl.pending_pairings.len();
    wl.pending_pairings.retain(|e| match DateTime::parse_from_rfc3339(&e.issued_at) {
        Ok(ts) => ts.with_timezone(&Utc) > cutoff,
        Err(_) => true,
    });
    let pruned = wl.pending_pairings.len() != before;

    // Re-use an existing (non-expired) code if one is already pending for this chat.
    let (code, added) = if let Some(entry) = wl.pending_pairings.iter().find(|e| e.chat_id == chat_id.0) {
        (entry.code.clone(), false)
    } else {
        let code = generate_code();
        wl.pending_pairings.push(PairingEntry {
            code:      code.clone(),
            chat_id:   chat_id.0,
            issued_at: Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
        });
        (code, true)
    };

    // Persist if we added a new code or pruned expired ones.
    if added || pruned {
        if let Err(e) = save_wl(&shared.secrets_dir, &wl).await {
            error!(error = %e, "telegram: failed to write whitelist file");
        }
    }
    if added {
        info!(chat_id = chat_id.0, code = %code, "TELEGRAM PAIRING: code written to telegram_whitelist.json");
    }

    bot.send_message(
        chat_id,
        format!(
            "🔐 <b>Pairing required.</b>\n\n\
             Code: <code>{code}</code>\n\n\
             Provide this code to the web agent to authorize access.",
        ),
    )
    .parse_mode(ParseMode::Html)
    .await
    .ok();
}

pub(crate) fn generate_code() -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    (0..6).map(|_| CHARS[rng.random_range(0..CHARS.len())] as char).collect()
}

// ── Whitelist watchdog ────────────────────────────────────────────────────────
//
// Polls telegram_whitelist.json every 10 s for mtime changes.
// When a new chat_id appears in `whitelist` (agent moved it from pending),
// sends a welcome message so the user knows they are authorized.

pub(crate) async fn whitelist_watchdog(bot: Bot, secrets_dir: PathBuf, cancel: CancellationToken) {
    let path = secrets_dir.join("telegram_whitelist.json");

    let mut last_mtime: Option<SystemTime> = tokio::fs::metadata(&path).await.ok()
        .and_then(|m| m.modified().ok());
    let mut known_wl = load_wl(&secrets_dir).await.whitelist;

    let mut interval = tokio::time::interval(Duration::from_secs(10));
    interval.tick().await; // skip the immediate first tick

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {
                let new_mtime = tokio::fs::metadata(&path).await.ok()
                    .and_then(|m| m.modified().ok());
                if new_mtime.is_none() || new_mtime == last_mtime {
                    continue;
                }
                last_mtime = new_mtime;

                let wl = load_wl(&secrets_dir).await;
                let newly_authorized: Vec<i64> = wl.whitelist.iter()
                    .filter(|id| !known_wl.contains(id))
                    .cloned()
                    .collect();

                if !newly_authorized.is_empty() {
                    info!(users = ?newly_authorized, "telegram: new users authorized — sending welcome");
                    for &chat_id in &newly_authorized {
                        bot.send_message(
                            ChatId(chat_id),
                            "✅ <b>Access granted!</b>\n\
                             You can now talk to your agent.\n\n\
                             /help for available commands.",
                        )
                        .parse_mode(ParseMode::Html)
                        .await
                        .ok();
                    }
                }

                known_wl = wl.whitelist;
                info!(
                    whitelist  = known_wl.len(),
                    pending    = wl.pending_pairings.len(),
                    "telegram: whitelist file reloaded"
                );
            }
        }
    }
}
