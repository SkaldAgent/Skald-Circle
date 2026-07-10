//! The three LLM-callable control tools (plugin.md §11).
//!
//! These implement `core_api::tool::Tool` and close over the plugin's
//! `RelayAgent`. The host (main crate) registers them in its `ToolRegistry` via
//! [`mobile_tools`]. `mobile_start_pairing` is gated behind the approval engine
//! the same way `execute_cmd`/`restart` are: the host seeds a `require` rule for
//! the tool name (see docs), so opening a pairing window is always a deliberate
//! human action and never triggerable by prompt injection.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};

use core_api::tool::{Tool, ToolCategory};

use crate::agent::{ClientState, RelayAgent};

/// Tool name constants (also the patterns the host uses for approval rules).
pub const TOOL_START_PAIRING: &str = "mobile_start_pairing";
pub const TOOL_LIST_DEVICES: &str = "mobile_list_devices";
pub const TOOL_REVOKE_DEVICE: &str = "mobile_revoke_device";

/// Build the plugin's LLM tools, bound to a `RelayAgent`. The host calls this
/// and registers the result in its `ToolRegistry`.
pub fn mobile_tools(agent: Arc<dyn RelayAgent>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(StartPairingTool { agent: Arc::clone(&agent) }),
        Arc::new(ListDevicesTool { agent: Arc::clone(&agent) }),
        Arc::new(RevokeDeviceTool { agent }),
    ]
}

// ── mobile_start_pairing ──────────────────────────────────────────────────────

struct StartPairingTool {
    agent: Arc<dyn RelayAgent>,
}

impl Tool for StartPairingTool {
    fn name(&self) -> &str { TOOL_START_PAIRING }
    fn description(&self) -> &str {
        "Open a pairing window so a new mobile device can be added, returning a URL \
         that renders a QR code to scan. The window auto-expires. Requires user approval."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ttl": {
                    "type": "integer",
                    "description": "Optional window lifetime in seconds (max 600). Defaults to the configured value."
                }
            }
        })
    }
    fn category(&self) -> ToolCategory { ToolCategory::Config }
    fn interactive_only(&self) -> bool { true }

    fn execute_async<'a>(
        &'a self,
        args: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let ttl = args.get("ttl").and_then(Value::as_u64).unwrap_or(0) as u32;
            let handle = self.agent.start_pairing(ttl).await?;
            Ok(format!(
                "Pairing window open. Show this QR to the device:\n\n![pairing QR]({})\n\n\
                 The window expires automatically.",
                handle.url
            ))
        })
    }
}

// ── mobile_list_devices ───────────────────────────────────────────────────────

struct ListDevicesTool {
    agent: Arc<dyn RelayAgent>,
}

impl Tool for ListDevicesTool {
    fn name(&self) -> &str { TOOL_LIST_DEVICES }
    fn description(&self) -> &str {
        "List paired mobile devices: state (pending/authorized), platform, device info, last seen."
    }
    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn category(&self) -> ToolCategory { ToolCategory::Introspection }

    fn execute_async<'a>(
        &'a self,
        _args: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let clients = self.agent.list_clients().await;
            if clients.is_empty() {
                return Ok("No paired devices.".to_string());
            }
            let items: Vec<Value> = clients
                .into_iter()
                .map(|c| {
                    let device_info: Option<Value> =
                        c.device_info.as_deref().and_then(|s| serde_json::from_str(s).ok());
                    json!({
                        "ed25519_pub": hex::encode(c.ed25519_pub),
                        "state": match c.state {
                            ClientState::Authorized => "authorized",
                            ClientState::Pending => "pending",
                        },
                        "platform": c.platform,
                        "device_info": device_info,
                        "last_seen": c.last_seen,
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&json!({ "devices": items }))?)
        })
    }
}

// ── mobile_revoke_device ──────────────────────────────────────────────────────

struct RevokeDeviceTool {
    agent: Arc<dyn RelayAgent>,
}

impl Tool for RevokeDeviceTool {
    fn name(&self) -> &str { TOOL_REVOKE_DEVICE }
    fn description(&self) -> &str {
        "Revoke a paired mobile device by its ed25519 public key (hex). The device loses access immediately."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pubkey": {
                    "type": "string",
                    "description": "The device's ed25519 public key, hex-encoded (64 chars)."
                }
            },
            "required": ["pubkey"]
        })
    }
    fn category(&self) -> ToolCategory { ToolCategory::Config }

    fn execute_async<'a>(
        &'a self,
        args: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let pubkey = args
                .get("pubkey")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("mobile_revoke_device: missing `pubkey`"))?;
            let ed = skald_relay_common::crypto::decode_hex::<32>(pubkey)
                .ok_or_else(|| anyhow::anyhow!("mobile_revoke_device: `pubkey` is not 32-byte hex"))?;
            self.agent.revoke_client(ed).await?;
            Ok(format!("Device {pubkey} revoked."))
        })
    }
}
