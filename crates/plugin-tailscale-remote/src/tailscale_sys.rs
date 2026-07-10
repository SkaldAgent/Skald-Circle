use std::net::Ipv4Addr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use async_trait::async_trait;
use tokio::process::Command;

use core_api::remote::RemoteAccess;

/// Remote-access provider that reads the Tailscale IP from the system daemon.
///
/// Requires `tailscaled` to be installed and running; the `tailscale` CLI must
/// be on PATH. No embedded netstack — zero experimental dependencies.
pub struct TailscaleSystemProvider {
    ip:        OnceLock<Ipv4Addr>,
    connected: AtomicBool,
}

impl TailscaleSystemProvider {
    /// Runs `tailscale ip -4`, caches the result, and returns `Self` on success.
    pub async fn new() -> Result<Self> {
        let provider = Self {
            ip:        OnceLock::new(),
            connected: AtomicBool::new(false),
        };
        let ip = provider.fetch_ip().await?;
        let _ = provider.ip.set(ip);
        provider.connected.store(true, Ordering::Relaxed);
        Ok(provider)
    }

    async fn fetch_ip(&self) -> Result<Ipv4Addr> {
        let output = Command::new("tailscale")
            .args(["ip", "-4"])
            .output()
            .await
            .context("tailscale command not found — is the Tailscale daemon installed and running?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("tailscale ip -4 failed: {stderr}");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let ip_str = stdout.trim();
        ip_str
            .parse::<Ipv4Addr>()
            .with_context(|| format!("failed to parse tailscale IP: '{ip_str}'"))
    }
}

#[async_trait]
impl RemoteAccess for TailscaleSystemProvider {
    fn provider_name(&self) -> &str { "tailscale_sys" }

    async fn device_ip(&self) -> Result<Ipv4Addr> {
        self.ip
            .get()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("tailscale_sys: not connected"))
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    async fn shutdown(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }
}
