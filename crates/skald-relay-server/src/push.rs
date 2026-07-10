//! Push bridge (APNs / FCM). The **normative**, testable part always lives here:
//! the content-in-push vs wake-only decision (relay.md §5, 3500-byte base64
//! threshold) and the JSON payload construction. The actual send to Apple/Google
//! sits behind the [`Pusher`] trait: the default [`LogPusher`] needs no
//! credentials (it logs a redacted decision), so the relay also boots locally.
//! Live senders sit behind the `push-live` feature.

use crate::limits::CONTENT_PUSH_MAX_B64;
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as B64};
use serde_json::{Value, json};

/// Device platform (relay-protocol.md): selects APNs vs FCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Ios,
    Android,
}

impl Platform {
    pub fn parse(s: &str) -> Option<Platform> {
        match s {
            "ios" => Some(Platform::Ios),
            "android" => Some(Platform::Android),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Platform::Ios => "ios",
            Platform::Android => "android",
        }
    }
}

/// Result of the push-mode decision (relay.md §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushKind {
    /// The encrypted blob fits the limit: include it (NSE/app decrypts E2E).
    Content,
    /// Blob too large: wake only; the device opens a WS and drains the queue.
    Wake,
}

/// Everything needed to build a push.
///
/// `ciphertext` carries the **raw** bytes — exactly what v2 transports on the
/// WebSocket (protobuf `Message.ciphertext`). The relay base64-encodes them
/// when building the APNs/FCM JSON envelope (`d.c` field): that field is still
/// base64 because that's what the APNs/FCM wire protocols expect, unchanged
/// from v1. The base64 step therefore lives **inside** `apns_payload()` /
/// `fcm_payload()` — callers never pre-encode.
#[derive(Debug, Clone)]
pub struct PushItem {
    pub namespace_id: String,
    pub from_hex: String,
    pub nonce_hex: String,
    /// Raw ciphertext bytes (v2 `Message.ciphertext` as it travels on the
    /// WebSocket). The relay base64-encodes them when building APNs/FCM JSON.
    pub ciphertext: Vec<u8>,
}

impl PushItem {
    /// Normative selection rule: content-in-push if
    /// `len(base64(ciphertext)) <= CONTENT_PUSH_MAX_B64`, otherwise wake-only.
    /// We measure the base64 length on demand (cheap, no allocation kept).
    pub fn kind(&self) -> PushKind {
        if B64.encode(&self.ciphertext).len() <= CONTENT_PUSH_MAX_B64 {
            PushKind::Content
        } else {
            PushKind::Wake
        }
    }

    /// APNs payload (relay.md §5.1/5.2). `aps.alert` is a generic fallback:
    /// **never** sensitive content.
    pub fn apns_payload(&self) -> Value {
        match self.kind() {
            PushKind::Content => json!({
                "aps": {
                    "alert": { "title": "Skald", "body": "Azione richiesta" },
                    "badge": 1,
                    "sound": "default",
                    "mutable-content": 1,
                    "category": "skald_inbox"
                },
                "d": {
                    "ns": self.namespace_id,
                    "from": self.from_hex,
                    "n": self.nonce_hex,
                    "c": B64.encode(&self.ciphertext)
                }
            }),
            PushKind::Wake => json!({
                "aps": {
                    "alert": { "title": "Skald", "body": "Azione richiesta" },
                    "badge": 1,
                    "sound": "default",
                    "content-available": 1
                },
                "d": { "ns": self.namespace_id, "wake": true }
            }),
        }
    }

    /// FCM HTTP v1 payload (relay.md §5.3): **data-only**, high priority, so the
    /// app always handles decryption even in the background.
    pub fn fcm_payload(&self, device_token: &str) -> Value {
        let mut data = serde_json::Map::new();
        data.insert("ns".into(), json!(self.namespace_id));
        match self.kind() {
            PushKind::Content => {
                data.insert("from".into(), json!(self.from_hex));
                data.insert("n".into(), json!(self.nonce_hex));
                data.insert("c".into(), json!(B64.encode(&self.ciphertext)));
            }
            PushKind::Wake => {
                data.insert("wake".into(), json!("true"));
            }
        }
        json!({
            "message": {
                "token": device_token,
                "android": { "priority": "high" },
                "data": Value::Object(data)
            }
        })
    }
}

/// Push-send abstraction. Implemented by [`LogPusher`] (default) and, behind the
/// `push-live` feature, by the real APNs/FCM senders.
#[async_trait]
pub trait Pusher: Send + Sync {
    async fn notify(&self, device_token: &str, platform: Platform, item: &PushItem);
}

/// Default pusher: sends nothing, only logs a redacted decision. Lets
/// store-and-forward work locally without Apple/Google credentials.
pub struct LogPusher;

#[async_trait]
impl Pusher for LogPusher {
    async fn notify(&self, device_token: &str, platform: Platform, item: &PushItem) {
        let kind = item.kind();
        // Never log the content: only metadata and truncated identifiers.
        tracing::info!(
            target: "relay::push",
            platform = platform.as_str(),
            kind = ?kind,
            ns = %short(&item.namespace_id),
            token = %short(device_token),
            ct_b64_len = B64.encode(&item.ciphertext).len(),
            "would deliver push (no push credentials configured: LogPusher)"
        );
    }
}

/// Truncate an identifier for logging (never log full sensitive strings).
fn short(s: &str) -> String {
    let n = s.len().min(8);
    format!("{}…", &s[..n])
}

// ---------------------------------------------------------------------------
// Live push senders (feature `push-live`). The normative decision/payload
// logic above stays feature-free and is what the unit tests cover; the real
// network calls to Apple/Google live behind the gate and need no test
// fixtures (they need real credentials).
// ---------------------------------------------------------------------------

#[cfg(feature = "push-live")]
mod live {
    use super::*;
    use crate::config::ApnsConfig;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    /// APNs HTTP/2 sender (provider-auth via ES256 JWT, per Apple docs). Caches
    /// the JWT in memory and refreshes it at 30 min (token is valid 60 min).
    pub struct ApnsPusher {
        config: ApnsConfig,
        jwt: tokio::sync::RwLock<JwtState>,
        client: reqwest::Client,
    }

    /// Cached provider-auth JWT. Re-signed lazily when the remaining TTL
    /// drops below [`REFRESH_AFTER`].
    struct JwtState {
        token: String,
        expires_at: Instant,
    }

    /// Refresh threshold (Apple allows up to 60 min; we renew at the halfway
    /// point so a clock-skew rejection is unlikely).
    const REFRESH_AFTER: Duration = Duration::from_secs(30 * 60);
    /// TTL Apple assigns to a freshly issued provider JWT.
    const JWT_TTL: Duration = Duration::from_secs(60 * 60);

    impl ApnsPusher {
        pub fn new(config: ApnsConfig) -> Self {
            let client = reqwest::Client::new();
            let jwt = tokio::sync::RwLock::new(JwtState {
                token: String::new(),
                // Start expired so the first `notify()` triggers a sign.
                expires_at: Instant::now(),
            });
            Self {
                config,
                jwt,
                client,
            }
        }

        /// Return a valid provider JWT, signing a fresh one if the cached one
        /// is within [`REFRESH_AFTER`] of its TTL.
        async fn jwt(&self) -> anyhow::Result<String> {
            // Fast path: cached token is still good.
            {
                let state = self.jwt.read().await;
                if state.expires_at.saturating_duration_since(Instant::now()) > REFRESH_AFTER {
                    return Ok(state.token.clone());
                }
            }
            // Slow path: take the write lock, double-check (another task may
            // have refreshed while we were waiting), then sign.
            let mut state = self.jwt.write().await;
            if state.expires_at.saturating_duration_since(Instant::now()) > REFRESH_AFTER {
                return Ok(state.token.clone());
            }
            let token = self.generate_jwt()?;
            state.token = token.clone();
            state.expires_at = Instant::now() + JWT_TTL;
            Ok(token)
        }

        /// Sign a fresh provider JWT (ES256 over the team's P-256 key).
        fn generate_jwt(&self) -> anyhow::Result<String> {
            let mut header = Header::new(Algorithm::ES256);
            header.kid = Some(self.config.key_id.clone());

            let iat = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;
            let claims = serde_json::json!({
                "iss": self.config.team_id,
                "iat": iat,
            });

            let key = EncodingKey::from_ec_pem(self.config.private_key_pem.as_bytes())?;
            Ok(encode(&header, &claims, &key)?)
        }

        /// POST the APNs payload over HTTP/2 (negotiated via ALPN by reqwest).
        async fn send_apns(&self, device_token: &str, item: &PushItem) -> anyhow::Result<()> {
            let token = self.jwt().await?;
            let host = if self.config.sandbox {
                "https://api.sandbox.push.apple.com"
            } else {
                "https://api.push.apple.com"
            };
            let url = format!("{host}/3/device/{device_token}");
            let push_type = match item.kind() {
                PushKind::Content => "alert",
                PushKind::Wake => "background",
            };
            let body = item.apns_payload();
            let apns_id = Uuid::new_v4().to_string();

            let resp = self
                .client
                .post(&url)
                .header("apns-topic", &self.config.bundle_id)
                .header("apns-push-type", push_type)
                .header("apns-id", &apns_id)
                .header("authorization", format!("bearer {token}"))
                .json(&body)
                .send()
                .await?;

            let status = resp.status();
            if !status.is_success() {
                // Apple returns a JSON `{"reason": "..."}` body on errors; safe
                // to log (it never echoes our payload content).
                let reason = resp.text().await.unwrap_or_default();
                tracing::warn!(
                    target: "relay::push",
                    status = %status,
                    apns_id = %apns_id,
                    reason = %reason,
                    "APNs request failed"
                );
            } else {
                tracing::info!(
                    target: "relay::push",
                    apns_id = %apns_id,
                    "APNs request accepted"
                );
            }
            Ok(())
        }
    }

    #[async_trait]
    impl Pusher for ApnsPusher {
        async fn notify(&self, device_token: &str, platform: Platform, item: &PushItem) {
            // Defense in depth: an empty token would build `/3/device/` and get
            // a `MissingDeviceToken` 400 from Apple. Callers already filter
            // these out, but never spend a request on a token we know is empty.
            if device_token.is_empty() {
                tracing::debug!(
                    target: "relay::push",
                    "skipping APNs send: empty device token"
                );
                return;
            }
            // FcmPusher is not implemented yet: this sender only knows APNs.
            if platform != Platform::Ios {
                tracing::debug!(
                    target: "relay::push",
                    platform = platform.as_str(),
                    "ApnsPusher ignoring non-iOS notification (no FcmPusher yet)"
                );
                return;
            }
            if let Err(e) = self.send_apns(device_token, item).await {
                // Never echo device_token or content — only the truncated
                // identifier and a generic error class.
                tracing::warn!(
                    target: "relay::push",
                    device_token = %short(device_token),
                    error = %e,
                    "APNs send failed"
                );
            }
        }
    }

    /// Build the live APNs pusher. Caller falls back to [`LogPusher`] if
    /// `cfg.apns` is `None` (key file missing, bundle id unset, …).
    pub fn build_pusher(cfg: &ApnsConfig) -> Arc<dyn Pusher> {
        Arc::new(ApnsPusher::new(cfg.clone()))
    }
}

#[cfg(feature = "push-live")]
pub use live::build_pusher;

#[cfg(test)]
mod tests {
    use super::*;

    fn item(ct_len: usize) -> PushItem {
        PushItem {
            namespace_id: "a".repeat(64),
            from_hex: "b".repeat(64),
            nonce_hex: "c".repeat(24),
            ciphertext: vec![0xAA; ct_len],
        }
    }

    #[test]
    fn threshold_is_inclusive_3500() {
        // 2625 raw bytes → 3500 base64 bytes (2625 % 3 == 0 → no padding needed).
        let at_boundary = item((CONTENT_PUSH_MAX_B64 / 4) * 3);
        assert_eq!(at_boundary.kind(), PushKind::Content);
        // 2626 raw bytes → 3504 base64 bytes (2626 % 3 == 1 → 2 padding chars) → Wake.
        assert_eq!(
            item((CONTENT_PUSH_MAX_B64 / 4) * 3 + 1).kind(),
            PushKind::Wake
        );
    }

    #[test]
    fn apns_content_has_blob_and_mutable() {
        let p = item(100).apns_payload();
        assert_eq!(p["aps"]["mutable-content"], 1);
        assert_eq!(p["d"]["c"], B64.encode(&vec![0xAA; 100]));
        assert_eq!(p["d"]["n"], "c".repeat(24));
        assert!(p["d"].get("wake").is_none());
    }

    #[test]
    fn apns_wake_has_no_content() {
        let p = item(CONTENT_PUSH_MAX_B64 + 50).apns_payload();
        assert_eq!(p["aps"]["content-available"], 1);
        assert_eq!(p["d"]["wake"], true);
        assert!(p["d"].get("c").is_none());
    }

    #[test]
    fn fcm_is_data_only_high_priority() {
        let p = item(100).fcm_payload("tok123");
        assert_eq!(p["message"]["token"], "tok123");
        assert_eq!(p["message"]["android"]["priority"], "high");
        assert_eq!(p["message"]["data"]["c"], B64.encode(&vec![0xAA; 100]));
        assert!(p["message"].get("notification").is_none());
    }
}
