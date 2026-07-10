use std::net::{Ipv4Addr, SocketAddr};
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::{info, warn};

use core_api::remote::RemoteAccess;

pub struct TailscaleConfig {
    /// Tailscale auth key (tskey-auth-...). Required for first join; reused from key file afterwards.
    pub auth_key: String,
    /// Hostname this node will request on the tailnet.
    pub hostname:  String,
    /// Path where tailscale-rs persists node keys between restarts.
    pub key_file:  String,
}

pub struct TailscaleEmbeddedProvider {
    device:    Mutex<Option<tailscale::Device>>,
    ip:        OnceLock<Ipv4Addr>,
    hostname:  String,
}

impl TailscaleEmbeddedProvider {
    pub async fn new(cfg: TailscaleConfig) -> Result<Self> {
        let mut ts_cfg = tailscale::Config::default_with_key_file(&cfg.key_file)
            .await
            .context("loading tailscale key file")?;

        ts_cfg.requested_hostname = Some(cfg.hostname.clone());

        let auth_key = if cfg.auth_key.is_empty() { None } else { Some(cfg.auth_key) };

        let device = tailscale::Device::new(&ts_cfg, auth_key)
            .await
            .context("creating tailscale device")?;

        let ip = device.ipv4_addr().await.context("getting tailscale IPv4")?;
        info!(provider = "tailscale", %ip, hostname = %cfg.hostname, "mesh device ready");

        Ok(Self {
            device:   Mutex::new(Some(device)),
            ip:       OnceLock::from(ip),
            hostname: cfg.hostname,
        })
    }

    /// Create an Axum-compatible listener bound to the mesh IP on `port`.
    /// The caller is responsible for spawning an `axum::serve` task with the result.
    pub async fn axum_listener(&self, port: u16) -> Result<tailscale::axum::Listener> {
        let ip   = self.ip.get().copied().ok_or_else(|| anyhow!("device not ready"))?;
        let addr = SocketAddr::from((ip, port));

        let guard  = self.device.lock().await;
        let device = guard.as_ref().ok_or_else(|| anyhow!("device shut down"))?;

        let net_listener = device
            .tcp_listen(addr)
            .await
            .with_context(|| format!("tcp_listen on {addr}"))?;

        Ok(tailscale::axum::Listener::from(net_listener))
    }
}

#[async_trait]
impl RemoteAccess for TailscaleEmbeddedProvider {
    fn provider_name(&self) -> &str { "tailscale" }

    async fn device_ip(&self) -> Result<Ipv4Addr> {
        self.ip.get().copied().ok_or_else(|| anyhow!("device not ready"))
    }

    fn is_connected(&self) -> bool { self.ip.get().is_some() }

    async fn shutdown(&self) {
        let device = self.device.lock().await.take();
        if let Some(dev) = device {
            let clean = dev.shutdown(Some(std::time::Duration::from_secs(5))).await;
            if clean {
                info!(provider = "tailscale", hostname = %self.hostname, "mesh device shut down cleanly");
            } else {
                warn!(provider = "tailscale", hostname = %self.hostname, "mesh device shutdown timed out");
            }
        }
    }
}
