use std::net::Ipv4Addr;

use anyhow::Result;
use async_trait::async_trait;

/// Abstraction over a mesh/remote-connectivity provider.
/// The core (AppState, plugins) talks only to this trait —
/// no Tailscale-specific types leak outside the implementation.
#[async_trait]
pub trait RemoteAccess: Send + Sync {
    /// Short provider name used in logs and status fields (e.g. `"tailscale"`).
    fn provider_name(&self) -> &str;

    /// IP address of this node on the mesh network.
    async fn device_ip(&self) -> Result<Ipv4Addr>;

    /// True once the provider has joined the mesh and obtained an IP.
    fn is_connected(&self) -> bool;

    /// Graceful shutdown — disconnect from the mesh and release resources.
    async fn shutdown(&self);
}
