use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use rand::RngExt;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::tools::ToolResult;

pub use mcp_client::{
    ElicitationHandler,
    McpCallResult, McpLogLine, McpLogTx, McpMedia, McpMediaData, McpMediaKind,
    McpServerClient, McpServerConfig, McpServerInfo, McpServerStatus, McpTool, McpTransport as McpTransportKind,
    parse_mcp_tool_name,
    http_server::McpHttpServer,
    server::{McpNotification, McpServer},
};

use mcp_client::McpTransport;

mod logs;

const SERVER_START_TIMEOUT_SECS: u64 = 120;

// ── McpManager ───────────────────────────────────────────────────────────────

pub struct McpManager {
    pool:            Arc<SqlitePool>,
    servers:         RwLock<HashMap<String, Arc<dyn McpServerClient>>>,
    errors:          RwLock<HashMap<String, String>>,
    descriptions:    RwLock<HashMap<String, Option<String>>>,
    notification_tx: mpsc::UnboundedSender<McpNotification>,
    /// Feeds per-server diagnostic lines (stderr, `notifications/message`,
    /// lifecycle) to the `logs::log_consumer`, which writes `logs/mcp/<name>.log`.
    log_tx:          McpLogTx,
    /// Bridges server-initiated `elicitation/create` requests to the Inbox.
    /// Set once via `set_elicitation_handler` before `initialize` runs.
    elicitation_handler: RwLock<Option<Arc<dyn ElicitationHandler>>>,
    /// Data root for persisting non-text tool-result media (`media_dir`).
    data_root:       PathBuf,
}

impl McpManager {
    pub fn new(pool: Arc<SqlitePool>, shutdown: CancellationToken, data_root: impl Into<PathBuf>) -> Self {
        let (notification_tx, notification_rx) = mpsc::unbounded_channel::<McpNotification>();
        let (log_tx, log_rx) = mpsc::unbounded_channel::<McpLogLine>();

        let pool_bg = pool.clone();
        tokio::spawn(Self::notification_consumer(pool_bg, notification_rx, shutdown.clone()));
        tokio::spawn(logs::log_consumer(log_rx, shutdown));

        Self {
            pool,
            servers:      RwLock::new(HashMap::new()),
            errors:       RwLock::new(HashMap::new()),
            descriptions: RwLock::new(HashMap::new()),
            notification_tx,
            log_tx,
            elicitation_handler: RwLock::new(None),
            data_root:    data_root.into(),
        }
    }

    /// Emits a lifecycle line to a server's per-server log file (start failure,
    /// timeout, connection). Used for transports that have no `stderr` of their
    /// own to carry connection diagnostics (notably HTTP/SSE).
    fn log_lifecycle(&self, server: &str, text: impl Into<String>) {
        let _ = self.log_tx.send(McpLogLine::lifecycle(server.to_string(), text));
    }

    /// Directory under the data root where inline tool-result media (images,
    /// audio, embedded resources) is persisted and served from `/api/mcp-media/`.
    pub fn media_dir(&self) -> PathBuf {
        self.data_root.join("mcp_media")
    }

    /// Wire the elicitation bridge. Must be called before `initialize` so that
    /// stdio servers are started with a handler for `elicitation/create`.
    pub fn set_elicitation_handler(&self, handler: Arc<dyn ElicitationHandler>) {
        *self.elicitation_handler.write().unwrap() = Some(handler);
    }

    fn elicitation_handler(&self) -> Option<Arc<dyn ElicitationHandler>> {
        self.elicitation_handler.read().unwrap().clone()
    }

    async fn notification_consumer(
        pool:     Arc<SqlitePool>,
        mut rx:   mpsc::UnboundedReceiver<McpNotification>,
        shutdown: CancellationToken,
    ) {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("mcp: notification consumer shutdown");
                    break;
                }
                msg = rx.recv() => match msg {
                    Some((source, payload)) => {
                        let method  = payload["method"].as_str().unwrap_or("unknown").to_string();
                        let params  = serde_json::to_string(&payload["params"]).unwrap_or_else(|_| "{}".to_string());
                        match crate::db::mcp_events::insert(&pool, &source, &method, &params).await {
                            Ok(id) => info!("mcp_event stored: id={id} source={source} method={method}"),
                            Err(e) => warn!("mcp_events insert failed (source={source} method={method}): {e}"),
                        }
                    }
                    None => break,
                }
            }
        }
    }

    fn cfg_from_row(row: &crate::db::mcp_servers::McpServerRow) -> McpServerConfig {
        McpServerConfig {
            name:      row.name.clone(),
            transport: match row.transport.as_str() {
                "http" => McpTransport::Http,
                "sse"  => McpTransport::Sse,
                _      => McpTransport::Stdio,
            },
            command: row.command.clone(),
            args:    Some(row.args()).filter(|v| !v.is_empty()),
            env:     Some(row.env()).filter(|m| !m.is_empty()),
            url:     row.url.clone(),
            api_key: row.api_key.clone(),
        }
    }

    async fn start_one(
        cfg: &McpServerConfig,
        notification_tx: Option<mpsc::UnboundedSender<McpNotification>>,
        log_tx: Option<McpLogTx>,
        elicitation_handler: Option<Arc<dyn ElicitationHandler>>,
    ) -> Result<Arc<dyn McpServerClient>> {
        match cfg.transport {
            McpTransport::Stdio => {
                // Elicitation and per-server diagnostic capture (stderr +
                // notifications/message) are stdio-only. HTTP/SSE has no stderr and
                // no async notification stream, so it only gets lifecycle lines,
                // emitted by the manager (see `log_lifecycle`).
                McpServer::start(cfg, notification_tx, log_tx, elicitation_handler).await
                    .map(|s| Arc::new(s) as Arc<dyn McpServerClient>)
            }
            McpTransport::Http | McpTransport::Sse => {
                McpHttpServer::start(cfg).await
                    .map(|s| Arc::new(s) as Arc<dyn McpServerClient>)
            }
        }
    }

    pub async fn initialize(&self) {
        let rows = match crate::db::mcp_servers::all_enabled(&self.pool).await {
            Ok(r) => r,
            Err(e) => { warn!("McpManager::initialize: failed to read DB: {e}"); return; }
        };

        if rows.is_empty() {
            info!("No enabled MCP servers in DB — MCP disabled.");
            crate::boot::section("MCP servers — none enabled");
            return;
        }

        let cfgs: Vec<_> = rows.iter().map(Self::cfg_from_row).collect();
        {
            let mut descs = self.descriptions.write().unwrap();
            for row in &rows {
                descs.insert(row.name.clone(), row.description.clone());
            }
        }
        crate::boot::section(format!(
            "MCP servers — connecting to {} in background", cfgs.len()
        ));
        let handles: Vec<_> = cfgs.into_iter().map(|cfg| {
            let tx     = self.notification_tx.clone();
            let log_tx = self.log_tx.clone();
            let eh = self.elicitation_handler();
            tokio::spawn(async move {
                info!("MCP server '{}': starting…", cfg.name);
                let result = tokio::time::timeout(
                    Duration::from_secs(SERVER_START_TIMEOUT_SECS),
                    Self::start_one(&cfg, Some(tx), Some(log_tx), eh),
                ).await;
                (cfg.name, cfg.transport, result)
            })
        }).collect();

        for handle in handles {
            match handle.await {
                Ok((name, _, Ok(Ok(s)))) => {
                    let tool_names: Vec<_> = s.tools().iter().map(|t| t.name.as_str()).collect();
                    info!("MCP server '{}' ready — {} tool(s): {}", name, tool_names.len(), tool_names.join(", "));
                    let n = tool_names.len();
                    crate::boot::ok(format!("{name} ({n} tool{})", if n == 1 { "" } else { "s" }));
                    self.log_lifecycle(&name, format!("connected — {n} tool(s)"));
                    self.servers.write().unwrap().insert(name, s);
                }
                Ok((name, _, Ok(Err(e)))) => {
                    warn!("MCP server '{}' failed to start: {e}", name);
                    crate::boot::fail(format!("{name} — {e}"));
                    self.log_lifecycle(&name, format!("failed to start: {e}"));
                    self.errors.write().unwrap().insert(name, e.to_string());
                }
                Ok((name, _, Err(_))) => {
                    let msg = format!("startup timed out after {SERVER_START_TIMEOUT_SECS}s");
                    warn!("MCP server '{}' {msg}", name);
                    crate::boot::fail(format!("{name} — {msg}"));
                    self.log_lifecycle(&name, &msg);
                    self.errors.write().unwrap().insert(name, msg);
                }
                Err(e) => { warn!("MCP startup task panicked: {e}"); }
            }
        }
    }

    pub async fn register(&self, p: crate::db::mcp_servers::UpsertParams<'_>) -> Result<Vec<String>> {
        let name = p.name.to_string();

        crate::db::mcp_servers::upsert(&self.pool, p).await?;

        let rows = crate::db::mcp_servers::all_enabled(&self.pool).await?;
        let row = rows.into_iter().find(|r| r.name == name)
            .ok_or_else(|| anyhow::anyhow!("register: server '{}' not found after upsert", name))?;
        let cfg = Self::cfg_from_row(&row);

        let client = tokio::time::timeout(
            Duration::from_secs(SERVER_START_TIMEOUT_SECS),
            Self::start_one(&cfg, Some(self.notification_tx.clone()), Some(self.log_tx.clone()), self.elicitation_handler()),
        ).await
        .map_err(|_| {
            self.log_lifecycle(&name, "timed out during connection");
            anyhow::anyhow!("MCP server '{}' timed out during connection", name)
        })?
        .map_err(|e| {
            self.log_lifecycle(&name, format!("failed to start: {e}"));
            anyhow::anyhow!("MCP server '{}' failed to start: {e}", name)
        })?;

        let tool_names: Vec<String> = client.tools().iter().map(|t| t.name.clone()).collect();
        self.log_lifecycle(&name, format!("connected — {} tool(s)", tool_names.len()));
        self.errors.write().unwrap().remove(&name);
        self.descriptions.write().unwrap().insert(name.clone(), row.description.clone());
        self.servers.write().unwrap().insert(name, client);

        Ok(tool_names)
    }

    pub async fn unregister(&self, name: &str) -> Result<()> {
        crate::db::mcp_servers::delete(&self.pool, name).await?;
        self.servers.write().unwrap().remove(name);
        self.errors.write().unwrap().remove(name);
        self.descriptions.write().unwrap().remove(name);
        Ok(())
    }

    pub async fn set_enabled(&self, name: &str, enabled: bool) -> Result<()> {
        crate::db::mcp_servers::set_enabled(&self.pool, name, enabled).await
    }

    pub async fn list(&self) -> Result<Vec<McpServerInfo>> {
        let rows = crate::db::mcp_servers::all(&self.pool).await?;
        let servers = self.servers.read().unwrap();
        let errors  = self.errors.read().unwrap();

        let infos = rows.into_iter().map(|row| {
            let status = if !row.enabled {
                McpServerStatus::Disabled
            } else if let Some(s) = servers.get(&row.name) {
                McpServerStatus::Running {
                    tools: s.tools().iter().map(|t| t.name.clone()).collect(),
                }
            } else if let Some(e) = errors.get(&row.name) {
                McpServerStatus::Error { message: e.clone() }
            } else {
                McpServerStatus::Error { message: "not connected".to_string() }
            };
            McpServerInfo {
                name: row.name,
                transport: row.transport,
                description: row.description,
                friendly_name: row.friendly_name,
                status,
            }
        }).collect();

        Ok(infos)
    }

    pub fn tools(&self) -> Vec<McpTool> {
        self.servers.read().unwrap().values()
            .flat_map(|s| s.tools().iter().cloned())
            .collect()
    }

    pub fn tools_for(&self, names: &[String]) -> Vec<McpTool> {
        self.servers.read().unwrap().iter()
            .filter(|(name, _)| names.contains(name))
            .flat_map(|(_, s)| s.tools().iter().cloned())
            .collect()
    }

    pub fn server_descriptions(&self) -> HashMap<String, Option<String>> {
        self.descriptions.read().unwrap().clone()
    }

    pub fn server_infos(&self) -> Vec<Value> {
        self.servers.read().unwrap().iter()
            .map(|(name, s)| json!({
                "name": name,
                "tools": s.tools().iter().map(|t| json!({
                    "name":        t.name,
                    "description": t.description,
                })).collect::<Vec<_>>(),
            }))
            .collect()
    }

    pub async fn call(&self, server: &str, tool: &str, args: Value) -> Result<ToolResult> {
        let s = self.servers.read().unwrap()
            .get(server)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("MCP server '{server}' not found"))?;
        match s.call_tool(tool, args).await? {
            McpCallResult::Text(t) => Ok(ToolResult::Text(t)),
            McpCallResult::Json(v) => Ok(ToolResult::Json(v)),
            McpCallResult::Media { text, structured, items } =>
                Ok(ToolResult::Text(self.persist_media(server, text, structured, items).await)),
            // Experimental Tasks — defensive fallback. Normally the transport's
            // `call_tool` polls a deferred task to completion (block-and-poll) and
            // returns the real result, so this arm is not hit. It only surfaces a
            // raw handle if polling was bypassed, so the result is never lost.
            McpCallResult::Task(t) => {
                let ttl = t.ttl_ms.map(|ms| format!(", ttl {}s", ms / 1000)).unwrap_or_default();
                Ok(ToolResult::Text(format!(
                    "MCP server '{server}' deferred this call as task `{}` (status: {:?}{ttl}). \
                     Task polling is not implemented yet, so the result can't be retrieved automatically.",
                    t.task_id, t.status,
                )))
            }
        }
    }

    /// Persists the inline media of an MCP tool result under [`media_dir`] and
    /// composes a markdown text result that references each item by URL — so the
    /// model can surface it (the frontend renders the markdown) instead of the
    /// bytes being silently dropped. `resource_link`s are passed through by URI
    /// without downloading. Falls back to a textual placeholder if a write fails,
    /// so a disk error never loses the rest of the result.
    async fn persist_media(
        &self,
        server:     &str,
        text:       Option<String>,
        structured: Option<Value>,
        items:      Vec<McpMedia>,
    ) -> String {
        let mut out: Vec<String> = Vec::new();
        if let Some(t) = text.filter(|t| !t.is_empty()) {
            out.push(t);
        }

        for item in items {
            match item.data {
                McpMediaData::Inline { bytes, mime } => {
                    let file = format!("{}.{}", random_id(), ext_for_mime(&mime));
                    let dir  = self.media_dir();
                    let saved = async {
                        tokio::fs::create_dir_all(&dir).await?;
                        tokio::fs::write(dir.join(&file), &bytes).await
                    }.await;
                    match saved {
                        Ok(()) => {
                            let url = format!("/api/mcp-media/{file}");
                            let kb  = bytes.len().div_ceil(1024);
                            out.push(match item.kind {
                                McpMediaKind::Image    => format!("![image]({url}) ({mime}, {kb} KB)"),
                                McpMediaKind::Audio    => format!("[audio]({url}) ({mime}, {kb} KB)"),
                                McpMediaKind::Resource => format!("[file]({url}) ({mime}, {kb} KB)"),
                            });
                        }
                        Err(e) => {
                            warn!("MCP '{server}': failed to persist tool-result media: {e}");
                            out.push(format!("[media not saved: {mime}]"));
                        }
                    }
                }
                McpMediaData::Link { uri, mime } => {
                    let label = mime.as_deref().unwrap_or("resource");
                    out.push(format!("[{label}]({uri})"));
                }
            }
        }

        if let Some(sc) = structured {
            if let Ok(s) = serde_json::to_string_pretty(&sc) {
                out.push(format!("```json\n{s}\n```"));
            }
        }

        out.join("\n\n")
    }
}

/// Generates a 32-char alphanumeric id for a persisted media filename
/// (mirrors `ImageGeneratorManager`).
fn random_id() -> String {
    rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

/// Maps a MIME type to a file extension for persisted MCP media; `bin` for unknown.
pub fn ext_for_mime(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or("").trim() {
        "image/png"        => "png",
        "image/jpeg"       => "jpg",
        "image/gif"        => "gif",
        "image/webp"       => "webp",
        "image/svg+xml"    => "svg",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/mpeg"       => "mp3",
        "audio/ogg"        => "ogg",
        "video/mp4"        => "mp4",
        "video/webm"       => "webm",
        "application/pdf"  => "pdf",
        "application/json" => "json",
        "text/plain"       => "txt",
        _                  => "bin",
    }
}

/// Inverse of [`ext_for_mime`] for serving persisted media with the right
/// `Content-Type`; generic binary for unknown extensions.
pub fn content_type_for_ext(ext: &str) -> &'static str {
    match ext {
        "png"  => "image/png",
        "jpg"  => "image/jpeg",
        "gif"  => "image/gif",
        "webp" => "image/webp",
        "svg"  => "image/svg+xml",
        "wav"  => "audio/wav",
        "mp3"  => "audio/mpeg",
        "ogg"  => "audio/ogg",
        "mp4"  => "video/mp4",
        "webm" => "video/webm",
        "pdf"  => "application/pdf",
        "json" => "application/json",
        "txt"  => "text/plain",
        _      => "application/octet-stream",
    }
}
