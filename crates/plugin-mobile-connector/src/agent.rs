//! The `RelayAgent` control surface (plugin.md §4). This is the domain API the
//! UI / control tools use for pairing, listing devices, and revoking. It is NOT
//! an LLM tool itself: the three LLM tools (tools.rs) call into it.

use async_trait::async_trait;

/// Returned by `start_pairing`. The `code` is a random handle distinct from the
/// `pairing_token`; it identifies the in-memory session at the QR endpoint.
pub struct PairingHandle {
    /// e.g. `/api/plugin/mobile-connector/pairingqrcode?code=<random>`
    pub url: String,
    pub code: String,
    /// Unix ms.
    pub expires_at: i64,
}

/// Device authorization state, surfaced to the listing tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientState {
    Pending,
    Authorized,
}

/// One device, surfaced for `mobile_list_devices`.
pub struct ClientInfo {
    pub ed25519_pub: [u8; 32],
    pub x25519_pub: [u8; 32],
    pub state: ClientState,
    /// Raw device_info JSON (from `hello`), if received.
    pub device_info: Option<String>,
    pub platform: Option<String>,
    /// Unix ms of last activity, if any.
    pub last_seen: Option<i64>,
    /// The Skald user this device is bound to, if any (blueprint §13).
    pub bound_user: Option<String>,
}

/// The control API exposed by the plugin. Reachable via
/// `PluginManager::get_plugin_typed::<MobileConnectorPlugin>()`.
#[async_trait]
pub trait RelayAgent: Send + Sync {
    /// Open the pairing window (single-window, latest-wins) and return the
    /// auto-expiring QR URL.
    async fn start_pairing(&self, ttl_secs: u32) -> anyhow::Result<PairingHandle>;

    /// Close the pairing window.
    async fn stop_pairing(&self) -> anyhow::Result<()>;

    /// ed25519 public key (namespace identity).
    fn agent_ed25519_pub(&self) -> [u8; 32];

    /// Derived namespace id (hex).
    fn namespace_id(&self) -> String;

    /// List all known devices, each tagged with its bound user (if any).
    async fn list_clients(&self) -> Vec<ClientInfo>;

    /// Bind a paired device to a Skald user and authorize it (blueprint §13,
    /// admin-mediated — the mobile analogue of `telegram_pairing`).
    async fn bind_device(
        &self,
        ed25519_pub: [u8; 32],
        user_id: String,
        display: Option<String>,
    ) -> anyhow::Result<()>;

    /// Revoke a device (lost/stolen) by its ed25519 pubkey and drop its binding.
    async fn revoke_client(&self, ed25519_pub: [u8; 32]) -> anyhow::Result<()>;
}
