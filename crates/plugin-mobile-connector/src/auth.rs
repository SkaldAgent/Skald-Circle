//! Device→user binding (blueprint §13), the mobile analogue of the Telegram
//! `chat_id ↔ user_id` pairing.
//!
//! One relay identity serves many devices; each **authorized device** (identified
//! by its ed25519 public key) is bound to exactly one Skald user. The binding
//! lives in the `config` table (key `"mobile-connector"`) as JSON, so it survives
//! restarts and is admin-editable. Writing it emits a `ConfigKeyUpdated` event,
//! which [`config_listener`] uses to refresh the in-memory cache and (re)spawn
//! per-user forwarders — no polling, no restart.
//!
//! Unlike Telegram there is no "pairing code" concept here: the QR pairing and the
//! Pending/Authorized device lifecycle are handled by `skald-relay-client`. A
//! binding simply ties an already-paired pubkey to a user (admin-mediated, via the
//! `mobile_bind_device` tool — the mobile analogue of `telegram_pairing`).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tracing::{info, warn};

use core_api::config_api::ConfigApi;
use core_api::system_bus::SystemEvent;

use crate::PLUGIN_ID;
use crate::app::RelayApp;

/// Config-table key under which all mobile bindings are stored as JSON.
pub(crate) const CONFIG_KEY: &str = "mobile-connector";

// ── Bindings schema (stored as JSON in the `config` table) ────────────────────

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
pub struct MobileConfig {
    #[serde(default)]
    pub bindings: Vec<Binding>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Binding {
    /// The device's ed25519 public key, hex-encoded (64 chars) — the stable,
    /// opaque device identity the relay registry keys on.
    pub pubkey_hex: String,
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

impl MobileConfig {
    /// The user bound to `pubkey_hex`, if any.
    pub fn user_for_pubkey(&self, pubkey_hex: &str) -> Option<String> {
        self.bindings
            .iter()
            .find(|b| b.pubkey_hex.eq_ignore_ascii_case(pubkey_hex))
            .map(|b| b.user_id.clone())
    }

    /// Hex pubkeys of every device bound to `user_id`.
    pub fn pubkeys_for_user(&self, user_id: &str) -> Vec<String> {
        self.bindings
            .iter()
            .filter(|b| b.user_id == user_id)
            .map(|b| b.pubkey_hex.clone())
            .collect()
    }

    /// The distinct user ids that have at least one bound device.
    pub fn bound_user_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.bindings.iter().map(|b| b.user_id.clone()).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Insert or replace the binding for a pubkey (a device belongs to one user).
    pub fn upsert(&mut self, binding: Binding) {
        self.bindings
            .retain(|b| !b.pubkey_hex.eq_ignore_ascii_case(&binding.pubkey_hex));
        self.bindings.push(binding);
    }

    /// Remove the binding for `pubkey_hex`. Returns true if one was removed.
    pub fn remove(&mut self, pubkey_hex: &str) -> bool {
        let before = self.bindings.len();
        self.bindings
            .retain(|b| !b.pubkey_hex.eq_ignore_ascii_case(pubkey_hex));
        self.bindings.len() != before
    }
}

// ── Config-table read/write ────────────────────────────────────────────────────

/// Reads the mobile config from the `config` table. Returns `Default` when the key
/// is absent or unparseable (never fails the caller).
pub(crate) async fn load_config(config: &dyn ConfigApi) -> anyhow::Result<MobileConfig> {
    match config.get(CONFIG_KEY).await? {
        Some(json) => Ok(serde_json::from_str(&json).unwrap_or_default()),
        None => Ok(MobileConfig::default()),
    }
}

/// Writes the mobile config to the `config` table. `ConfigApi::set` emits a
/// `ConfigKeyUpdated` event when the value changes, so [`config_listener`] and the
/// in-memory cache pick it up automatically.
pub(crate) async fn save_config(config: &dyn ConfigApi, cfg: &MobileConfig) -> anyhow::Result<()> {
    config
        .set(CONFIG_KEY, &serde_json::to_string_pretty(cfg)?)
        .await
}

// ── Config listener ────────────────────────────────────────────────────────────

/// Subscribes to the system bus and reloads the in-memory bindings whenever the
/// `"mobile-connector"` config key changes, then (re)spawns forwarders for every
/// bound + unlocked user. The mobile analogue of Telegram's `config_listener`.
pub(crate) async fn config_listener(
    app: Arc<RelayApp>,
    mut rx: broadcast::Receiver<SystemEvent>,
) {
    let cancel = app.cancel();
    info!(plugin = PLUGIN_ID, "config listener started");
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!(plugin = PLUGIN_ID, "config listener stopped");
                return;
            }
            result = rx.recv() => match result {
                Ok(SystemEvent::ConfigKeyUpdated { key, new_value, .. }) if key == CONFIG_KEY => {
                    match serde_json::from_str::<MobileConfig>(&new_value) {
                        Ok(cfg) => {
                            let n = cfg.bindings.len();
                            *app.bindings_mut().await = cfg;
                            info!(plugin = PLUGIN_ID, bindings = n, "bindings reloaded from config event");
                            crate::events::spawn_forwarders_for_bound_users(&app).await;
                        }
                        Err(e) => warn!(plugin = PLUGIN_ID, error = %e, "failed to parse config from event"),
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(plugin = PLUGIN_ID, skipped = n, "config listener lagged");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(pubkey_hex: &str, user_id: &str) -> Binding {
        Binding { pubkey_hex: pubkey_hex.to_string(), user_id: user_id.to_string(), display: None }
    }

    #[test]
    fn upsert_replaces_by_pubkey_case_insensitive() {
        let mut cfg = MobileConfig::default();
        cfg.upsert(binding("AABB", "user-a"));
        // Re-binding the same device (different hex case) to another user replaces,
        // never duplicates — a device belongs to exactly one user.
        cfg.upsert(binding("aabb", "user-b"));
        assert_eq!(cfg.bindings.len(), 1);
        assert_eq!(cfg.user_for_pubkey("aabb").as_deref(), Some("user-b"));
    }

    #[test]
    fn user_for_pubkey_is_case_insensitive() {
        let mut cfg = MobileConfig::default();
        cfg.upsert(binding("DEADbeef", "alice"));
        assert_eq!(cfg.user_for_pubkey("deadBEEF").as_deref(), Some("alice"));
        assert_eq!(cfg.user_for_pubkey("0000"), None);
    }

    #[test]
    fn pubkeys_for_user_scopes_to_owner() {
        let mut cfg = MobileConfig::default();
        cfg.upsert(binding("aa", "alice"));
        cfg.upsert(binding("bb", "alice"));
        cfg.upsert(binding("cc", "bob"));
        let mut alice = cfg.pubkeys_for_user("alice");
        alice.sort();
        assert_eq!(alice, vec!["aa".to_string(), "bb".to_string()]);
        assert_eq!(cfg.pubkeys_for_user("bob"), vec!["cc".to_string()]);
        assert!(cfg.pubkeys_for_user("carol").is_empty());
    }

    #[test]
    fn bound_user_ids_dedup_sorted() {
        let mut cfg = MobileConfig::default();
        cfg.upsert(binding("aa", "bob"));
        cfg.upsert(binding("bb", "alice"));
        cfg.upsert(binding("cc", "bob"));
        assert_eq!(cfg.bound_user_ids(), vec!["alice".to_string(), "bob".to_string()]);
    }

    #[test]
    fn remove_reports_and_is_case_insensitive() {
        let mut cfg = MobileConfig::default();
        cfg.upsert(binding("AbCd", "alice"));
        assert!(cfg.remove("abcd"));
        assert!(!cfg.remove("abcd"));
        assert!(cfg.bindings.is_empty());
    }
}
