use std::sync::Arc;

use chrono::{DateTime, Local, Utc};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use teloxide::prelude::*;
use teloxide::types::ParseMode;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use core_api::config_api::ConfigApi;
use core_api::system_bus::SystemEvent;

use super::TgShared;

/// Config-table key under which all Telegram bindings are stored as JSON.
pub(crate) const CONFIG_KEY: &str = "telegram";

// ── Bindings schema (stored as JSON in the `config` table) ────────────────────

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct TelegramConfig {
    #[serde(default)]
    pub bindings: Vec<Binding>,
    #[serde(default)]
    pub pending_pairings: Vec<PairingEntry>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Binding {
    pub chat_id: i64,
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PairingEntry {
    pub code:      String,
    pub chat_id:   i64,
    pub issued_at: String,
}

// ── Config-table read/write ────────────────────────────────────────────────────

/// Reads the Telegram config from the `config` table. Returns `Default` when
/// the key is absent or unparseable (never fails the caller).
pub(crate) async fn load_config(config: &dyn ConfigApi) -> anyhow::Result<TelegramConfig> {
    match config.get(CONFIG_KEY).await? {
        Some(json) => Ok(serde_json::from_str(&json).unwrap_or_default()),
        None       => Ok(TelegramConfig::default()),
    }
}

/// Writes the Telegram config to the `config` table. `ConfigApi::set` emits a
/// `ConfigKeyUpdated` event when the value changes, so the in-memory cache and
/// any forwarders are updated automatically.
pub(crate) async fn save_config(
    config: &dyn ConfigApi,
    cfg: &TelegramConfig,
) -> anyhow::Result<()> {
    config.set(CONFIG_KEY, &serde_json::to_string_pretty(cfg)?).await
}

// ── Pairing ───────────────────────────────────────────────────────────────────

/// Pairing codes older than this are considered abandoned and pruned.
const PAIRING_TTL_HOURS: i64 = 24;

/// Called when an unbound `chat_id` sends a message. Generates (or reuses) a
/// pairing code, persists it to the config table, and replies with instructions.
pub(crate) async fn handle_pairing(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    let mut cfg = shared.bindings.read().await.clone();

    // Prune expired codes.
    let cutoff = Utc::now() - chrono::Duration::hours(PAIRING_TTL_HOURS);
    cfg.pending_pairings.retain(|e| match DateTime::parse_from_rfc3339(&e.issued_at) {
        Ok(ts) => ts.with_timezone(&Utc) > cutoff,
        Err(_) => true,
    });

    // Reuse an existing code for this chat, or generate a new one.
    let (code, added) = if let Some(entry) = cfg.pending_pairings.iter().find(|e| e.chat_id == chat_id.0) {
        (entry.code.clone(), false)
    } else {
        let code = generate_code();
        cfg.pending_pairings.push(PairingEntry {
            code:      code.clone(),
            chat_id:   chat_id.0,
            issued_at: Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string(),
        });
        (code, true)
    };

    if added {
        if let Err(e) = save_config(&*shared.config, &cfg).await {
            error!(error = %e, "telegram: failed to write pairing to config table");
        } else {
            // Update the in-memory cache immediately (the config_listener will
            // also fire, but this avoids a race if the user sends another
            // message before the event arrives).
            *shared.bindings.write().await = cfg.clone();
        }
        info!(chat_id = chat_id.0, code = %code, "TELEGRAM PAIRING: code written to config table");
    }

    bot.send_message(
        chat_id,
        format!(
            "🔐 <b>Pairing required.</b>\n\n\
             Code: <code>{code}</code>\n\n\
             Ask the admin to authorize this chat using the telegram_pairing tool.",
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

// ── Config listener ────────────────────────────────────────────────────────────

/// Subscribes to the system bus and reloads the in-memory bindings whenever the
/// `"telegram"` config key changes. Replaces the old file-polling watchdog.
pub(crate) async fn config_listener(
    shared:     Arc<TgShared>,
    mut rx:     broadcast::Receiver<SystemEvent>,
    cancel:     CancellationToken,
) {
    info!("telegram: config listener started");
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("telegram: config listener stopped");
                return;
            }
            result = rx.recv() => match result {
                Ok(SystemEvent::ConfigKeyUpdated { key, new_value, .. }) if key == CONFIG_KEY => {
                    match serde_json::from_str::<TelegramConfig>(&new_value) {
                        Ok(cfg) => {
                            let n = cfg.bindings.len();
                            *shared.bindings.write().await = cfg;
                            info!(bindings = n, "telegram: bindings reloaded from config event");
                        }
                        Err(e) => warn!(error = %e, "telegram: failed to parse config from event"),
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "telegram: config listener lagged");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}
