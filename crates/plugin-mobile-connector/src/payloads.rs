//! E2E payload schemas (payloads.md). These are the JSON plaintexts sealed into
//! the `ciphertext` field of a `message` envelope; the relay never sees them.
//!
//! `request_id` travels as a decimal STRING of the i64 rowid (plugin.md §2): we
//! serialize i64 → string outbound, and parse string → i64 inbound, dropping
//! anything that does not parse.

use chrono::Utc;
use serde_json::Value;

use core_api::inbox::InboxSnapshot;

/// Generate a v4-ish UUID string for the payload `id` field. We avoid pulling in
/// the `uuid` crate: a 16-byte CSPRNG value formatted as a UUID is sufficient
/// for dedup/ack purposes (payloads.md §1).
fn new_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    // Set version (4) and variant bits for a well-formed UUID.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15],
    )
}

/// Convert an ISO-8601 UTC timestamp string into unix milliseconds. Falls back
/// to "now" if the string fails to parse (payloads.md wants an int ms field).
fn iso_to_ms(iso: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or_else(|_| Utc::now().timestamp_millis())
}

// ── Agent → Client ──────────────────────────────────────────────────────────

/// Build the `inbox_update` snapshot payload (payloads.md §3.1) from an
/// `InboxSnapshot`. Mirrors the Inbox 1:1; `badge` = total pending.
pub fn build_inbox_update(snapshot: &InboxSnapshot) -> Value {
    let approvals: Vec<Value> = snapshot
        .approvals
        .iter()
        .map(|a| {
            serde_json::json!({
                "request_id": a.request_id.to_string(),
                "tool_name":  a.tool_name,
                "agent_label": "Skald",
                // Short human label for card/notification; raw args for the detail
                // dialog (untruncated — e.g. the full `execute_cmd` command).
                "summary":    a.summary,
                "arguments":  a.arguments,
                "created_at": iso_to_ms(&a.created_at),
            })
        })
        .collect();

    let clarifications: Vec<Value> = snapshot
        .clarifications
        .iter()
        .map(|c| {
            serde_json::json!({
                "request_id":  c.request_id.to_string(),
                "question":    c.question,
                "context":     c.context_label,
                "suggested_answers": c.suggested_answers,
                "agent_label": "Skald",
                "created_at":  iso_to_ms(&c.created_at),
            })
        })
        .collect();

    // MCP server-initiated input requests (e.g. an SSH/sudo password). We ship
    // only the prompt metadata — never the value; the value is supplied by the
    // device in `elicitation_response.content` and travels E2E (payloads.md §3.1).
    let elicitations: Vec<Value> = snapshot
        .elicitations
        .iter()
        .map(|e| {
            serde_json::json!({
                "request_id":      e.request_id.to_string(),
                "server_name":     e.server_name,
                "message":         e.message,
                "field_name":      e.field_name,        // Option<String> → null if absent
                "sensitive":       e.sensitive,
                "is_confirmation": e.is_confirmation,
                "created_at":      iso_to_ms(&e.created_at),
            })
        })
        .collect();

    serde_json::json!({
        "v": 1,
        "kind": "inbox_update",
        "id": new_id(),
        "ts": Utc::now().timestamp_millis(),
        "badge": snapshot.total,
        "approvals": approvals,
        "clarifications": clarifications,
        "elicitations": elicitations,
    })
}

/// Build a generic `notification` payload (payloads.md §3.2). Part of the wire
/// contract; retained for the protocol surface even though the current per-user
/// flow pushes Inbox updates rather than free-form notifications.
#[allow(dead_code)]
pub fn build_notification(title: &str, body: &str) -> Value {
    serde_json::json!({
        "v": 1,
        "kind": "notification",
        "id": new_id(),
        "ts": Utc::now().timestamp_millis(),
        "title": title,
        "body": body,
    })
}

/// Build a `bind_result` payload — the agent's reply to a `bind_request`
/// (self-service device binding). `ok=true` carries the bound `user`; `ok=false`
/// carries an `error` string (invalid/expired session, bind failure). The device
/// uses it to confirm the pairing or to prompt the user to sign in again.
pub fn build_bind_result(ok: bool, user: Option<&str>, error: Option<&str>) -> Value {
    serde_json::json!({
        "v": 1,
        "kind": "bind_result",
        "id": new_id(),
        "ts": Utc::now().timestamp_millis(),
        "ok": ok,
        "user": user,     // Option<&str> → null when absent
        "error": error,
    })
}

/// Build a `needs_unlock` payload — sent when a device acts for a user whose
/// database is locked (§9). It tells the app to run the login/unlock handshake
/// (`POST /api/auth/login` through the loopback proxy) rather than treating the
/// dropped request as a hard failure.
pub fn build_needs_unlock() -> Value {
    serde_json::json!({
        "v": 1,
        "kind": "needs_unlock",
        "id": new_id(),
        "ts": Utc::now().timestamp_millis(),
    })
}

// ── Client → Agent ──────────────────────────────────────────────────────────

/// A decoded client→agent payload (payloads.md §4). Only the fields the agent
/// acts on are modeled; unknown kinds become [`ClientPayload::Unknown`].
#[derive(Debug)]
pub enum ClientPayload {
    /// `hello`: device_info carried E2E.
    Hello { device_info: Value },
    /// `approval_response`.
    ApprovalResponse { request_id: i64, approved: bool, reason: Option<String> },
    /// `clarification_response`.
    ClarificationResponse { request_id: i64, answer: String },
    /// `elicitation_response`: the device's reply to an MCP elicitation. `action`
    /// is `"accept"`/`"decline"`/`"cancel"`; `content` (present only for `accept`)
    /// is an object keyed by `field_name` whose value may be a secret — never log it.
    ElicitationResponse { request_id: i64, action: String, content: Option<Value> },
    /// `inbox_request`: client asks for the current Inbox snapshot (payloads.md
    /// §4.6). Sent after every `auth_ok`; the agent replies with a targeted
    /// `inbox_update`. No fields beyond the common envelope.
    InboxRequest,
    /// `bind_request`: self-service device binding. The device presents the
    /// `session_token` it obtained from `POST /api/auth/login`; the agent
    /// resolves it to a user and binds this device's pubkey to them (no admin
    /// step). The token is a bearer credential — never log it.
    BindRequest { session_token: String },
    /// `logout`: device removes itself.
    Logout,
    /// Anything else (ack, unknown kind, malformed request_id) — ignored.
    Unknown,
}

/// Parse a decrypted client payload. Enforces `v == 1` and required-field
/// presence; on malformed input returns [`ClientPayload::Unknown`] (never panics,
/// payloads.md §6).
pub fn parse_client_payload(plaintext: &[u8]) -> ClientPayload {
    let Ok(v) = serde_json::from_slice::<Value>(plaintext) else {
        return ClientPayload::Unknown;
    };
    if v.get("v").and_then(Value::as_u64) != Some(1) {
        return ClientPayload::Unknown;
    }
    let kind = v.get("kind").and_then(Value::as_str).unwrap_or("");
    match kind {
        "hello" => match v.get("device_info") {
            Some(di) if di.is_object() => ClientPayload::Hello { device_info: di.clone() },
            _ => ClientPayload::Unknown,
        },
        "approval_response" => {
            let Some(rid) = parse_request_id(&v) else { return ClientPayload::Unknown };
            match v.get("decision").and_then(Value::as_str) {
                Some("approved") => ClientPayload::ApprovalResponse { request_id: rid, approved: true, reason: None },
                Some("rejected") => ClientPayload::ApprovalResponse {
                    request_id: rid,
                    approved: false,
                    reason: v.get("reason").and_then(Value::as_str).map(str::to_string),
                },
                _ => ClientPayload::Unknown,
            }
        }
        "clarification_response" => {
            let Some(rid) = parse_request_id(&v) else { return ClientPayload::Unknown };
            match v.get("answer").and_then(Value::as_str) {
                Some(answer) => ClientPayload::ClarificationResponse { request_id: rid, answer: answer.to_string() },
                None => ClientPayload::Unknown,
            }
        }
        "elicitation_response" => {
            let Some(rid) = parse_request_id(&v) else { return ClientPayload::Unknown };
            let action = match v.get("action").and_then(Value::as_str) {
                Some(a @ ("accept" | "decline" | "cancel")) => a.to_string(),
                _ => return ClientPayload::Unknown,
            };
            // `content` is meaningful only for `accept` and must be an object
            // (keyed by `field_name`); anything else is dropped.
            let content = v.get("content").filter(|c| c.is_object()).cloned();
            ClientPayload::ElicitationResponse { request_id: rid, action, content }
        }
        "inbox_request" => ClientPayload::InboxRequest,
        "bind_request" => match v.get("session_token").and_then(Value::as_str) {
            Some(tok) if !tok.is_empty() => ClientPayload::BindRequest { session_token: tok.to_string() },
            _ => ClientPayload::Unknown,
        },
        "logout" => ClientPayload::Logout,
        _ => ClientPayload::Unknown,
    }
}

/// Parse the `request_id` decimal string into an i64 (plugin.md §2).
fn parse_request_id(v: &Value) -> Option<i64> {
    v.get("request_id").and_then(Value::as_str)?.parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `accept` carries the secret under `content`, keyed by `field_name`.
    #[test]
    fn elicitation_response_accept_with_content() {
        let raw = br#"{
            "v": 1, "kind": "elicitation_response", "id": "abc", "ts": 1750000000000,
            "request_id": "123", "action": "accept",
            "content": { "password": "hunter2" }
        }"#;
        match parse_client_payload(raw) {
            ClientPayload::ElicitationResponse { request_id, action, content } => {
                assert_eq!(request_id, 123);
                assert_eq!(action, "accept");
                let content = content.expect("accept must carry content");
                assert_eq!(content["password"], "hunter2");
            }
            other => panic!("expected ElicitationResponse, got {other:?}"),
        }
    }

    /// `decline`/`cancel` have no `content`; a missing object yields `None`.
    #[test]
    fn elicitation_response_decline_without_content() {
        let raw = br#"{
            "v": 1, "kind": "elicitation_response", "id": "abc", "ts": 1750000000000,
            "request_id": "7", "action": "decline"
        }"#;
        match parse_client_payload(raw) {
            ClientPayload::ElicitationResponse { request_id, action, content } => {
                assert_eq!(request_id, 7);
                assert_eq!(action, "decline");
                assert!(content.is_none());
            }
            other => panic!("expected ElicitationResponse, got {other:?}"),
        }
    }

    /// A non-object `content` is dropped rather than forwarded.
    #[test]
    fn elicitation_response_non_object_content_dropped() {
        let raw = br#"{
            "v": 1, "kind": "elicitation_response", "id": "abc", "ts": 1750000000000,
            "request_id": "9", "action": "accept", "content": "not-an-object"
        }"#;
        match parse_client_payload(raw) {
            ClientPayload::ElicitationResponse { content, .. } => assert!(content.is_none()),
            other => panic!("expected ElicitationResponse, got {other:?}"),
        }
    }

    /// An unknown `action` is rejected as `Unknown` (no resolution attempted).
    #[test]
    fn elicitation_response_bad_action_is_unknown() {
        let raw = br#"{
            "v": 1, "kind": "elicitation_response", "id": "abc", "ts": 1750000000000,
            "request_id": "1", "action": "approve"
        }"#;
        assert!(matches!(parse_client_payload(raw), ClientPayload::Unknown));
    }

    /// A missing/non-string `request_id` is rejected as `Unknown`.
    #[test]
    fn elicitation_response_missing_request_id_is_unknown() {
        let raw = br#"{
            "v": 1, "kind": "elicitation_response", "id": "abc", "ts": 1750000000000,
            "action": "accept", "content": { "x": "y" }
        }"#;
        assert!(matches!(parse_client_payload(raw), ClientPayload::Unknown));
    }

    /// `bind_request` carries the session token the device logged in with.
    #[test]
    fn bind_request_parses_session_token() {
        let raw = br#"{
            "v": 1, "kind": "bind_request", "id": "abc", "ts": 1750000000000,
            "session_token": "tok-123"
        }"#;
        match parse_client_payload(raw) {
            ClientPayload::BindRequest { session_token } => assert_eq!(session_token, "tok-123"),
            other => panic!("expected BindRequest, got {other:?}"),
        }
    }

    /// A missing or empty `session_token` is rejected as `Unknown` (never binds).
    #[test]
    fn bind_request_missing_or_empty_token_is_unknown() {
        let missing = br#"{ "v": 1, "kind": "bind_request", "id": "a", "ts": 1 }"#;
        let empty   = br#"{ "v": 1, "kind": "bind_request", "id": "a", "ts": 1, "session_token": "" }"#;
        assert!(matches!(parse_client_payload(missing), ClientPayload::Unknown));
        assert!(matches!(parse_client_payload(empty),   ClientPayload::Unknown));
    }

    /// `bind_result` shape: ok carries the user; failure carries the error.
    #[test]
    fn bind_result_shape() {
        let ok = build_bind_result(true, Some("u1"), None);
        assert_eq!(ok["kind"], "bind_result");
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["user"], "u1");
        assert!(ok["error"].is_null());

        let err = build_bind_result(false, None, Some("invalid or expired session"));
        assert_eq!(err["ok"], false);
        assert!(err["user"].is_null());
        assert_eq!(err["error"], "invalid or expired session");
    }

    /// `needs_unlock` is a bare envelope the app reacts to by (re)logging in.
    #[test]
    fn needs_unlock_shape() {
        let p = build_needs_unlock();
        assert_eq!(p["kind"], "needs_unlock");
        assert_eq!(p["v"], 1);
    }
}
