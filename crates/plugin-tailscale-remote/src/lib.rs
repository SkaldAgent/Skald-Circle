use std::net::Ipv4Addr;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use core_api::plugin::{PluginContext, RouterFactory};
use core_api::remote::RemoteAccess;

#[cfg(feature = "remote-tailscale")]
mod tailscale;
mod tailscale_sys;

#[cfg(feature = "remote-tailscale")]
use tailscale::{TailscaleConfig, TailscaleEmbeddedProvider};

pub struct RemotePlugin {
    running:          Arc<AtomicBool>,
    cancel:           Mutex<Option<CancellationToken>>,
    handle:           Mutex<Option<JoinHandle<()>>>,
    /// Cached mesh IP — set synchronously once connected, cleared on stop.
    mesh_ip:          std::sync::Mutex<Option<Ipv4Addr>>,
    /// Active provider name cached for synchronous runtime_status().
    cached_provider:  std::sync::Mutex<Option<String>>,
    /// Concrete embedded provider kept alive for graceful shutdown.
    #[cfg(feature = "remote-tailscale")]
    provider: Mutex<Option<Arc<TailscaleEmbeddedProvider>>>,

    // ── Deps extracted from AppState on first start() ─────────────────────────
    // Using OnceLock so extraction is idempotent across reload() calls.
    // After the first start(), the internal methods use these and never
    // touch Arc<AppState> again.
    port:           OnceLock<u16>,
    remote_slot:    OnceLock<Arc<RwLock<Option<Arc<dyn RemoteAccess>>>>>,
    router_factory: OnceLock<RouterFactory>,
}

impl RemotePlugin {
    pub fn new() -> Self {
        Self {
            running:         Arc::new(AtomicBool::new(false)),
            cancel:          Mutex::new(None),
            handle:          Mutex::new(None),
            mesh_ip:         std::sync::Mutex::new(None),
            cached_provider: std::sync::Mutex::new(None),
            #[cfg(feature = "remote-tailscale")]
            provider: Mutex::new(None),
            port:           OnceLock::new(),
            remote_slot:    OnceLock::new(),
            router_factory: OnceLock::new(),
        }
    }

    /// Cache the three deps we need from PluginContext. Idempotent (OnceLock).
    fn extract_deps(&self, ctx: &PluginContext) {
        let _ = self.port.set(ctx.web_port);
        let _ = self.remote_slot.set(Arc::clone(&ctx.remote_slot));
        let _ = self.router_factory.set(Arc::clone(&ctx.router_factory));
    }
}

#[async_trait]
impl core_api::plugin::Plugin for RemotePlugin {
    fn id(&self)          -> &str { "remote_connectivity" }
    fn name(&self)        -> &str { "Remote Connectivity" }
    fn description(&self) -> &str {
        "Exposes the web app on a mesh network (Tailscale) so remote clients can connect \
         without port forwarding or internet exposure."
    }
    fn is_running(&self)  -> bool { self.running.load(Ordering::Relaxed) }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "provider": {
                    "type":        "string",
                    "enum":        ["tailscale_sys", "tailscale"],
                    "default":     "tailscale_sys",
                    "title":       "Provider",
                    "description": "tailscale_sys: uses the system Tailscale daemon (recommended). tailscale: experimental embedded Tailscale, no daemon required."
                },
                "auth_key": {
                    "type":        "string",
                    "title":       "Auth Key",
                    "description": "Tailscale auth key (tskey-auth-...). Only required for the embedded 'tailscale' provider on first join.",
                    "sensitive":   true
                },
                "hostname": {
                    "type":        "string",
                    "default":     "personal-agent",
                    "title":       "Hostname",
                    "description": "Hostname this node requests on the tailnet. Only used by the embedded 'tailscale' provider."
                },
                "key_file": {
                    "type":        "string",
                    "default":     "data/tailscale_keys.json",
                    "title":       "Key File",
                    "description": "Path for persisting node identity between restarts. Only used by the embedded 'tailscale' provider."
                }
            }
        })
    }

    fn runtime_status(&self) -> Option<Value> {
        if !self.running.load(Ordering::Relaxed) {
            return None;
        }
        let ip = self.mesh_ip.lock().ok()
            .and_then(|g| g.map(|ip| ip.to_string()))
            .unwrap_or_default();
        let provider = self.cached_provider.lock().ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(|| "unknown".to_string());
        Some(json!({ "provider": provider, "ip": ip, "connected": true }))
    }

    fn as_any(&self) -> &dyn std::any::Any { self }
    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> { self }

    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()> {
        self.extract_deps(&ctx);
        match (enabled, self.is_running()) {
            (true,  false) => self.start_with_config(config).await,
            (false, true)  => self.stop().await,
            (true,  true)  => { self.stop().await?; self.start_with_config(config).await }
            (false, false) => Ok(()),
        }
    }

    async fn start(&self, ctx: PluginContext) -> Result<()> {
        self.extract_deps(&ctx);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(token) = self.cancel.lock().await.take() {
            token.cancel();
        }
        if let Some(h) = self.handle.lock().await.take() {
            let _ = h.await;
        }

        #[cfg(feature = "remote-tailscale")]
        if let Some(provider) = self.provider.lock().await.take() {
            provider.shutdown().await;
        }

        if let Ok(mut g) = self.mesh_ip.lock() { *g = None; }
        if let Ok(mut g) = self.cached_provider.lock() { *g = None; }
        self.running.store(false, Ordering::Relaxed);
        info!(plugin = "remote_connectivity", "remote plugin stopped");
        Ok(())
    }
}

// ── Internal start helpers ────────────────────────────────────────────────────

impl RemotePlugin {
    async fn start_with_config(&self, config: Value) -> Result<()> {
        let provider_name = config["provider"].as_str().unwrap_or("tailscale_sys");
        match provider_name {
            "tailscale_sys" => self.start_tailscale_sys().await,
            #[cfg(feature = "remote-tailscale")]
            "tailscale" => self.start_tailscale(config).await,
            other => bail!("remote_connectivity: unknown provider '{other}'"),
        }
    }

    #[cfg(feature = "remote-tailscale")]
    async fn start_tailscale(&self, config: Value) -> Result<()> {
        let auth_key = config["auth_key"].as_str().unwrap_or("").to_string();
        let hostname = config["hostname"].as_str().unwrap_or("personal-agent").to_string();
        let key_file = config["key_file"]
            .as_str()
            .unwrap_or("data/tailscale_keys.json")
            .to_string();

        let port           = *self.port.get().expect("extract_deps not called");
        let remote_slot    = self.remote_slot.get().expect("extract_deps not called");
        let router_factory = self.router_factory.get().expect("extract_deps not called");

        let provider = Arc::new(
            TailscaleEmbeddedProvider::new(TailscaleConfig { auth_key, hostname, key_file }).await?
        );
        let ip = provider.device_ip().await?;

        if let Ok(mut g) = self.mesh_ip.lock()         { *g = Some(ip); }
        if let Ok(mut g) = self.cached_provider.lock() { *g = Some("tailscale".into()); }
        *remote_slot.write().await = Some(Arc::clone(&provider) as Arc<dyn RemoteAccess>);

        let listener = provider.axum_listener(port).await?;
        let router   = router_factory();
        let cancel   = CancellationToken::new();
        let cancel_c = cancel.clone();
        let running  = Arc::clone(&self.running);

        self.running.store(true, Ordering::Relaxed);

        let handle = tokio::spawn(async move {
            info!(provider = "tailscale", %ip, port, "mesh server listening");
            tokio::select! {
                _ = cancel_c.cancelled() => {
                    info!(provider = "tailscale", "mesh server cancelled");
                }
                result = axum::serve(listener, router) => {
                    match result {
                        Ok(())  => warn!(provider = "tailscale", "mesh server exited unexpectedly"),
                        Err(e)  => error!(provider = "tailscale", error = %e, "mesh server error"),
                    }
                }
            }
            running.store(false, Ordering::Relaxed);
        });

        *self.cancel.lock().await   = Some(cancel);
        *self.handle.lock().await   = Some(handle);
        *self.provider.lock().await = Some(provider);

        Ok(())
    }

    async fn start_tailscale_sys(&self) -> Result<()> {
        use tailscale_sys::TailscaleSystemProvider;

        let port           = *self.port.get().expect("extract_deps not called");
        let remote_slot    = self.remote_slot.get().expect("extract_deps not called");
        let router_factory = self.router_factory.get().expect("extract_deps not called");

        let provider = Arc::new(TailscaleSystemProvider::new().await?);
        let ip = provider.device_ip().await?;

        if let Ok(mut g) = self.mesh_ip.lock()         { *g = Some(ip); }
        if let Ok(mut g) = self.cached_provider.lock() { *g = Some("tailscale_sys".into()); }
        *remote_slot.write().await = Some(Arc::clone(&provider) as Arc<dyn RemoteAccess>);

        let listener   = tokio::net::TcpListener::bind((ip, port)).await?;
        let router     = router_factory();
        let cancel     = CancellationToken::new();
        let cancel_c   = cancel.clone();
        let running    = Arc::clone(&self.running);
        let provider_c = Arc::clone(&provider);

        self.running.store(true, Ordering::Relaxed);

        let handle = tokio::spawn(async move {
            info!(provider = "tailscale_sys", %ip, port, "mesh server listening");
            tokio::select! {
                _ = cancel_c.cancelled() => {
                    info!(provider = "tailscale_sys", "mesh server cancelled");
                }
                result = axum::serve(listener, router) => {
                    match result {
                        Ok(())  => warn!(provider = "tailscale_sys", "mesh server exited unexpectedly"),
                        Err(e)  => error!(provider = "tailscale_sys", error = %e, "mesh server error"),
                    }
                }
            }
            provider_c.shutdown().await;
            running.store(false, Ordering::Relaxed);
        });

        *self.cancel.lock().await = Some(cancel);
        *self.handle.lock().await = Some(handle);

        Ok(())
    }
}
