//! In-memory pairing window (relay-protocol.md §5, §6).
//!
//! A pairing session is transient (≤ TTL) and is NEVER persisted: it holds a
//! live `pairing_token` whose bytes must not touch disk (crypto.md §9). The map
//! `code → session` is single-window, latest-wins: a new `start_pairing`
//! supersedes the previous session.
//!
//! The `code` is a random, non-enumerable handle distinct from the
//! `pairing_token`; it is what travels in the QR endpoint URL, so a URL that
//! leaks into `chat_history` is only a capability that self-revokes once the
//! window closes.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::Utc;
use rand::RngCore;
use serde::Serialize;

/// Lifecycle state of a pairing session, used to pick the QR-endpoint response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Window open, token not yet consumed.
    Active,
    /// A device paired against this session (token consumed).
    Consumed,
    /// A newer `start_pairing` replaced this session.
    Superseded,
}

/// The JSON object encoded INSIDE the QR (relay-protocol.md §5). Field
/// order/encoding is normative: all 32-byte values are lowercase hex (64 chars).
#[derive(Debug, Clone, Serialize)]
pub struct QrCodeData {
    pub v: u8,
    pub relay_url: String,
    pub namespace_id: String,
    pub agent_ed25519_pub: String,
    pub agent_x25519_pub: String,
    pub pairing_token: String,
}

/// One in-memory pairing session keyed by its `code`.
#[derive(Debug, Clone)]
pub struct PairingSession {
    /// The QR payload to render while the window is active.
    pub qr: QrCodeData,
    /// Raw pairing token (kept to correlate, never serialized except in `qr`).
    pub token: [u8; 32],
    /// Unix-ms expiry.
    pub expires_at: i64,
    pub state: SessionState,
}

impl PairingSession {
    /// Effective state at `now`, folding in TTL expiry on top of the stored state.
    pub fn effective_state(&self, now_ms: i64) -> SessionState {
        match self.state {
            SessionState::Active if now_ms >= self.expires_at => SessionState::Superseded, // expired ⇒ placeholder
            other => other,
        }
    }
}

/// Result of opening a pairing window.
pub struct StartedPairing {
    pub code: String,
    pub token: [u8; 32],
    pub expires_at: i64,
}

/// Single-window registry of pairing sessions.
#[derive(Default)]
pub struct PairingStore {
    inner: Mutex<HashMap<String, PairingSession>>,
}

impl PairingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a new window (latest-wins): every existing session is marked
    /// Superseded, then the new one is inserted Active. Returns the handle.
    pub fn start(
        &self,
        relay_url: &str,
        namespace_id: &str,
        agent_ed25519_pub: &[u8; 32],
        agent_x25519_pub: &[u8; 32],
        ttl_secs: u32,
    ) -> StartedPairing {
        let mut token = [0u8; 32];
        rand::rng().fill_bytes(&mut token);
        let mut code_bytes = [0u8; 16];
        rand::rng().fill_bytes(&mut code_bytes);
        let code = hex::encode(code_bytes);
        let expires_at = Utc::now().timestamp_millis() + (ttl_secs as i64) * 1000;

        let qr = QrCodeData {
            v: 1,
            relay_url: relay_url.to_string(),
            namespace_id: namespace_id.to_string(),
            agent_ed25519_pub: hex::encode(agent_ed25519_pub),
            agent_x25519_pub: hex::encode(agent_x25519_pub),
            pairing_token: hex::encode(token),
        };

        let mut map = self.inner.lock().unwrap();
        for s in map.values_mut() {
            if s.state == SessionState::Active {
                s.state = SessionState::Superseded;
            }
        }
        map.insert(
            code.clone(),
            PairingSession { qr, token, expires_at, state: SessionState::Active },
        );

        StartedPairing { code, token, expires_at }
    }

    /// Mark every active session Superseded (used by `stop_pairing`).
    pub fn supersede_all(&self) {
        let mut map = self.inner.lock().unwrap();
        for s in map.values_mut() {
            if s.state == SessionState::Active {
                s.state = SessionState::Superseded;
            }
        }
    }

    /// Mark the active session whose token matches as Consumed (after a device
    /// pairs). Returns true if one was found.
    pub fn consume_by_token(&self, token: &[u8; 32]) -> bool {
        let mut map = self.inner.lock().unwrap();
        for s in map.values_mut() {
            if s.state == SessionState::Active
                && skald_relay_common::crypto::ct_eq(&s.token, token)
            {
                s.state = SessionState::Consumed;
                return true;
            }
        }
        false
    }

    /// The token of the single currently-active session, if any. The relay echoes
    /// no token in `client_paired`, so the active session's token is what we
    /// consume on pairing.
    pub fn active_token(&self) -> Option<[u8; 32]> {
        let now = Utc::now().timestamp_millis();
        let map = self.inner.lock().unwrap();
        map.values()
            .find(|s| s.effective_state(now) == SessionState::Active)
            .map(|s| s.token)
    }

    /// Look up a session by code, returning its QR payload and effective state.
    pub fn lookup(&self, code: &str) -> Option<(QrCodeData, SessionState)> {
        let now = Utc::now().timestamp_millis();
        let map = self.inner.lock().unwrap();
        map.get(code).map(|s| (s.qr.clone(), s.effective_state(now)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with_active() -> (PairingStore, StartedPairing) {
        let s = PairingStore::new();
        let started = s.start("wss://relay", "ns", &[1u8; 32], &[2u8; 32], 300);
        (s, started)
    }

    #[test]
    fn single_window_latest_wins() {
        let (s, first) = store_with_active();
        let _second = s.start("wss://relay", "ns", &[1u8; 32], &[2u8; 32], 300);
        let (_, state) = s.lookup(&first.code).expect("first session present");
        assert_eq!(state, SessionState::Superseded, "opening a new window supersedes the old one");
    }

    #[test]
    fn consume_by_token_marks_consumed() {
        let (s, started) = store_with_active();
        let token = started.token;
        let (_, state) = s.lookup(&started.code).expect("present");
        assert_eq!(state, SessionState::Active);

        assert!(s.consume_by_token(&token));
        let (_, state) = s.lookup(&started.code).expect("present");
        assert_eq!(state, SessionState::Consumed);
    }

    #[test]
    fn consume_with_wrong_token_is_noop() {
        let (s, _started) = store_with_active();
        assert!(!s.consume_by_token(&[0u8; 32]));
    }

    #[test]
    fn stop_pairing_supersedes_active() {
        let (s, started) = store_with_active();
        s.supersede_all();
        let (_, state) = s.lookup(&started.code).expect("present");
        assert_eq!(state, SessionState::Superseded);
    }

    #[test]
    fn lookup_unknown_code_is_none() {
        let s = PairingStore::new();
        assert!(s.lookup("nope").is_none());
    }

    #[test]
    fn expired_active_session_reports_superseded() {
        let (s, started) = store_with_active();
        // Construct a session already in the past by inserting manually.
        let mut session = s.inner.lock().unwrap();
        let entry = session.get_mut(&started.code).expect("present");
        entry.expires_at = Utc::now().timestamp_millis() - 1;
        drop(session);
        let (_, state) = s.lookup(&started.code).expect("present");
        assert_eq!(state, SessionState::Superseded, "expiry folds into Superseded");
    }
}
