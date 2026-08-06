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

/// Reads the Telegram config from the `config` table.
///
/// An **absent** key is an empty config — that is the state of a fresh install.
/// An **unparseable** one is an error, deliberately: this used to be
/// `unwrap_or_default()`, which turned a blob the current schema cannot read
/// into "no bindings, no pending codes" — and since every writer here saves the
/// whole blob back, the next pairing message would then overwrite the file with
/// that default and every binding on the box would be gone for good. Failing
/// loudly leaves the value intact for a human to look at.
pub(crate) async fn load_config(config: &dyn ConfigApi) -> anyhow::Result<TelegramConfig> {
    match config.get(CONFIG_KEY).await? {
        Some(json) => serde_json::from_str(&json)
            .map_err(|e| anyhow::anyhow!(
                "telegram: the stored `{CONFIG_KEY}` config is not readable ({e}) — \
                 refusing to overwrite it; inspect the `config` table"
            )),
        None => Ok(TelegramConfig::default()),
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
///
/// **Reads the store, not `shared.bindings`.** The cache is refreshed from a
/// lossy 64-slot broadcast (`ConfigKeyUpdated`), so it may hold a pending code
/// the store no longer has — a dropped event is enough. That cache is right for
/// the hot `chat_id → user_id` lookup on every inbound message; it is wrong
/// here, because the reader on the other side of the pairing (the web page and
/// the `telegram_pairing` tool) resolves the code against the **store**, and a
/// code handed out from a stale cache is one that can never bind: the user gets
/// their code and the web answers "invalid or expired". Pairing happens once
/// per person, so the extra read costs nothing.
pub(crate) async fn handle_pairing(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    let mut cfg = match load_config(&*shared.config).await {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "telegram: cannot read the config to issue a pairing code");
            bot.send_message(chat_id, "⚠️ Pairing is unavailable right now — please ask the admin to check the server.")
                .await.ok();
            return;
        }
    };

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
        // A code the store did not accept is worse than no code: the user pastes
        // it, the web resolves it against the store, and the failure surfaces
        // there — far from the cause. Say so here instead.
        if let Err(e) = save_config(&*shared.config, &cfg).await {
            error!(error = %e, "telegram: failed to write pairing to config table");
            bot.send_message(chat_id, "⚠️ Could not start pairing (the server refused to store the code). Please try again, or ask the admin.")
                .await.ok();
            return;
        }
        // Update the in-memory cache immediately (the config_listener will
        // also fire, but this avoids a race if the user sends another
        // message before the event arrives).
        *shared.bindings.write().await = cfg.clone();
        info!(chat_id = chat_id.0, code = %code, "TELEGRAM PAIRING: code written to config table");
    }

    bot.send_message(
        chat_id,
        format!(
            "🔐 <b>Pairing required.</b>\n\n\
             Code: <code>{code}</code>\n\n\
             Open the Plugins page in the Skald web app and paste this code \
             to link your account (or ask the admin).",
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

/// Turns a pairing code into a binding for `user_id` (the web self-service
/// flow — `Plugin::update_user_config`). Mirrors the `telegram_pairing` tool's
/// bind semantics: the pending entry is consumed and any existing binding for
/// that chat is replaced. Returns the bound `chat_id`.
pub(crate) fn apply_pairing_code(
    cfg:     &mut TelegramConfig,
    code:    &str,
    user_id: &str,
) -> anyhow::Result<i64> {
    let code = code.trim();
    let pos = cfg.pending_pairings.iter()
        .position(|e| e.code.eq_ignore_ascii_case(code))
        .ok_or_else(|| anyhow::anyhow!("invalid or expired pairing code — send a message to the bot to get a new one"))?;
    let chat_id = cfg.pending_pairings.remove(pos).chat_id;
    cfg.bindings.retain(|b| b.chat_id != chat_id);
    cfg.bindings.push(Binding {
        chat_id,
        user_id: user_id.to_string(),
        display: None,
    });
    Ok(chat_id)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_pairing(code: &str, chat_id: i64) -> TelegramConfig {
        TelegramConfig {
            bindings:         vec![],
            pending_pairings: vec![PairingEntry {
                code:      code.to_string(),
                chat_id,
                issued_at: "2026-01-01T00:00:00+00:00".to_string(),
            }],
        }
    }

    #[test]
    fn pairing_code_creates_binding_and_consumes_entry() {
        let mut cfg = cfg_with_pairing("ABC123", 42);
        let chat_id = apply_pairing_code(&mut cfg, "ABC123", "u1").unwrap();
        assert_eq!(chat_id, 42);
        assert!(cfg.pending_pairings.is_empty(), "the code must be consumed");
        assert_eq!(cfg.bindings.len(), 1);
        assert_eq!(cfg.bindings[0].user_id, "u1");
        assert_eq!(cfg.bindings[0].chat_id, 42);
    }

    #[test]
    fn pairing_code_is_case_and_whitespace_insensitive() {
        let mut cfg = cfg_with_pairing("ABC123", 42);
        apply_pairing_code(&mut cfg, "  abc123 ", "u1").unwrap();
        assert_eq!(cfg.bindings[0].user_id, "u1");
    }

    #[test]
    fn pairing_replaces_an_existing_binding_for_the_same_chat() {
        let mut cfg = cfg_with_pairing("ABC123", 42);
        cfg.bindings.push(Binding { chat_id: 42, user_id: "old".into(), display: None });
        cfg.bindings.push(Binding { chat_id: 99, user_id: "other".into(), display: None });
        apply_pairing_code(&mut cfg, "ABC123", "u1").unwrap();
        assert_eq!(cfg.bindings.len(), 2);
        assert!(cfg.bindings.iter().any(|b| b.chat_id == 42 && b.user_id == "u1"));
        assert!(cfg.bindings.iter().any(|b| b.chat_id == 99 && b.user_id == "other"),
                "bindings for other chats are untouched");
    }

    /// A `ConfigApi` over one in-memory value, so the load path can be tested
    /// without a database.
    struct FakeConfig(Option<String>);

    #[async_trait::async_trait]
    impl ConfigApi for FakeConfig {
        async fn get(&self, _key: &str) -> anyhow::Result<Option<String>> { Ok(self.0.clone()) }
        async fn set(&self, _key: &str, _value: &str) -> anyhow::Result<()> { Ok(()) }
    }

    /// The distinction the silent `unwrap_or_default()` used to erase: an absent
    /// key is a fresh install, an unreadable one must not present itself as an
    /// empty config that the next write would then persist over the real one.
    #[tokio::test]
    async fn an_absent_key_is_empty_and_an_unreadable_one_is_an_error() {
        let empty = load_config(&FakeConfig(None)).await.unwrap();
        assert!(empty.bindings.is_empty() && empty.pending_pairings.is_empty());

        let err = load_config(&FakeConfig(Some("{ not json".into()))).await.unwrap_err();
        assert!(err.to_string().contains("not readable"), "got: {err}");

        // A blob from a future/other schema is unreadable too — `bindings` must
        // be an array of objects, and a wrong shape has to fail, not default.
        assert!(load_config(&FakeConfig(Some(r#"{"bindings":"nope"}"#.into()))).await.is_err());
    }

    #[test]
    fn unknown_code_fails_and_keeps_state() {
        let mut cfg = cfg_with_pairing("ABC123", 42);
        let err = apply_pairing_code(&mut cfg, "ZZZ999", "u1").unwrap_err();
        assert!(err.to_string().contains("invalid or expired"));
        assert_eq!(cfg.pending_pairings.len(), 1, "the pending entry must survive a failed attempt");
        assert!(cfg.bindings.is_empty());
    }
}
