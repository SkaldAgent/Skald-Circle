//! Connector marketplace — a remote feed of vetted connectors (blueprint §14/§15).
//!
//! The feed is **consultative, not authoritative**: it *proposes* connectors, the
//! admin *installs* one into `mcp_catalog`, and only then can it be enabled
//! globally (`mcp_global_servers`) or activated per-user (`mcp_user_servers`).
//! The trust anchor stays on the box, so §14's risk axis is untouched — importing
//! an `mcp_local` entry writes a script that will execute here, and therefore
//! still demands the admin-only `mcp.register_local_script` on top of
//! `mcp.manage_catalog`.
//!
//! Everything is fetched **server-side**: the feed serves no CORS headers, so a
//! browser cannot read it directly, and proxying also keeps the household's
//! browsing pattern off the open web (only the box's IP reaches the feed, and it
//! pulls the whole index rather than querying per connector).
//!
//! ## What the digests do and do not buy
//!
//! Each manifest declares a SHA-256 per file, and [`install`] refuses any file
//! that does not match — fail-closed. That is **pinning**, not authenticity: the
//! digest arrives over the same channel as the file, so whoever can serve a
//! modified script can serve its modified digest too. What it buys is that the
//! hash recorded at install time makes any *later* silent change detectable — no
//! quiet code update on the box. Real authenticity needs the index signed by a key
//! that does not live on the web server; the format is ready for it, the check is
//! not written yet.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::extract::{Extension, Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use skald_core::db::{mcp_catalog, role_capabilities};
use skald_core::skald::Skald;

use super::guard::AuthUser;
use super::ApiError;

/// The configured feed URL (`marketplace.url` in `config.yml`), installed by
/// [`crate::frontend::WebFrontend::new`] at startup.
static FEED_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Installs the feed URL from config. Called once during frontend construction;
/// later calls are ignored, so tests and the desktop shell cannot race it.
pub fn set_feed_url(url: String) {
    let _ = FEED_URL.set(url.trim_end_matches('/').to_string());
}

/// Where the feed lives. Falls back to the public host when nothing configured it
/// — a missing `marketplace:` block should degrade to the default, not to a
/// panic.
fn base_url() -> String {
    FEED_URL
        .get()
        .cloned()
        .unwrap_or_else(|| "https://connectors.skaldagent.net".to_string())
}

/// How long a hydrated feed stays warm. The feed changes rarely; the admin can
/// force a refetch from the UI.
const CACHE_TTL: Duration = Duration::from_secs(300);

/// Refuses a file the feed declares as absurdly large before downloading it. Files
/// are verified in memory, so this also bounds the allocation.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("skald/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("marketplace http client")
});

// ── the feed's wire format ────────────────────────────────────────────────────
//
// Deliberately tolerant: the feed evolves alongside this client, so every field
// the client can derive itself is optional and unknown fields are ignored. The
// feed's vocabulary is NOT Skald's — see `norm_*` below for the translation.

#[derive(Debug, Clone, Default, Deserialize)]
struct IndexEntry {
    id:               String,
    #[serde(default)] name:             Option<String>,
    /// The index is the single place that names files and their digests, which is
    /// what makes it the one document worth signing: verify it, and every artifact
    /// below is anchored. (A manifest cannot carry its own digest — writing the
    /// hash into the file changes the file.)
    #[serde(default)] files:            Vec<FileEntry>,
    /// `icon_small` is the current spelling; `small_icon` was the earlier one.
    #[serde(default, alias = "small_icon")] icon_small: Option<String>,
    #[serde(default, alias = "large_icon")] icon_large: Option<String>,
    #[serde(default)] user_description: Option<String>,
    #[serde(default)] requires:         Vec<String>,
    #[serde(default)] tags:             Vec<String>,
    #[serde(default)] folder:           Option<String>,
    /// `user` | `global` — the feed's word for §7 placement.
    #[serde(default)] scope:            Option<String>,
    /// `mcp_local` | `mcp_remote` — the §14 risk axis.
    #[serde(default, rename = "type")] kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Index {
    #[serde(default)] connectors: Vec<IndexEntry>,
}

#[derive(Debug, Clone, Deserialize)]
struct FileEntry {
    path:             String,
    sha256:           String,
    #[serde(default)] size: Option<u64>,
}

/// How a connector authenticates. `delivery` matters because Skald's remote
/// transport sends a key as `Authorization: Bearer`, while some servers (Tavily)
/// want it as a query parameter — which they express as a `{key}` placeholder in
/// the URL that `skald_core::mcp` substitutes at connect time. The placeholder is
/// what actually drives the substitution, so the feed's `param` name is not read.
#[derive(Debug, Clone, Default, Deserialize)]
struct AuthSpec {
    #[serde(default, rename = "type")] kind: Option<String>,
    #[serde(default)] delivery: Option<String>,
    #[serde(default)] scopes:   Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Doc {
    #[serde(default)] description: Option<String>,
    /// The text the LLM reads when deciding whether to `activate_tools()` on this
    /// server (tools are lazy-loaded), so it maps to `mcp_catalog.description` —
    /// the column that reaches the prompt. The human-facing blurb is
    /// `IndexEntry::user_description` and stays in the UI.
    #[serde(default)] llm_short_description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct McpConfigManifest {
    #[serde(default)] command:   Option<String>,
    #[serde(default)] args:      Vec<String>,
    #[serde(default)] env:       HashMap<String, String>,
    #[serde(default)] url:       Option<String>,
    #[serde(default)] transport: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Manifest {
    #[serde(default)] name:               Option<String>,
    #[serde(default)] version:            Option<String>,
    #[serde(default, rename = "type")] kind: Option<String>,
    #[serde(default)] transport:          Option<String>,
    #[serde(default)] requires:           Vec<String>,
    #[serde(default)] dependencies:       Vec<String>,
    #[serde(default)] setup_instructions: Vec<String>,
    #[serde(default)] docs:               Vec<Doc>,
    #[serde(default)] mcp_config:         Option<McpConfigManifest>,
    #[serde(default)] homepage:           Option<String>,
    /// Digests now live in the index (the signable root); kept here only so an
    /// older feed still installs.
    #[serde(default)] files:              Vec<FileEntry>,
    #[serde(default)] scope:              Option<String>,
    #[serde(default)] auth:               Option<AuthSpec>,
}

#[derive(Debug, Clone)]
struct Hydrated {
    entry:    IndexEntry,
    manifest: Manifest,
}

// ── feed vocabulary → Skald vocabulary ────────────────────────────────────────

/// §7 placement: does this run once for the household (host runtime) or once per
/// user (inside their container)? The feed says `user`, the catalog says
/// `per_user`.
///
/// Placement is **not** transport: a remote connector can be per-user (a personal
/// API key — `mcp.register_remote` exists precisely for that), and the inference
/// from `mcp_remote` → global only happens to hold for today's two entries. So the
/// feed's explicit `scope` always wins; the inference is a last resort, and it
/// resolves to `per_user`, the narrower blast radius.
fn norm_scope(entry: &IndexEntry, manifest: &Manifest) -> String {
    let declared = manifest.scope.as_deref().or(entry.scope.as_deref());
    match declared {
        Some("global") => "global".to_string(),
        Some("user") | Some("per_user") => "per_user".to_string(),
        _ => "per_user".to_string(),
    }
}

/// §14 risk axis: does installing this write code that will execute on the box?
/// `mcp_local` does, and that is the gated act — not "remote vs local" as such.
fn norm_source(entry: &IndexEntry, manifest: &Manifest) -> String {
    let declared = manifest.kind.as_deref().or(entry.kind.as_deref());
    match declared {
        Some("mcp_remote") => "remote".to_string(),
        Some("mcp_local") => "local_script".to_string(),
        // The index carries no `type` today. Fall back to the tags it does carry,
        // then to `local_script` — the answer that demands MORE authority, so a
        // silent misread cannot under-gate an install.
        _ if entry.tags.iter().any(|t| t == "remote") => "remote".to_string(),
        _ => "local_script".to_string(),
    }
}

/// `skald_core::mcp::transport_of` maps `http`→Http, `sse`→Sse and **everything
/// else to Stdio**. The feed says `streamable-http`, which would therefore fall
/// through to Stdio and try to spawn a command that does not exist — a silent
/// failure, not an error. Normalise here so only the three understood values are
/// ever stored.
fn norm_transport(manifest: &Manifest, source: &str) -> String {
    let declared = manifest
        .mcp_config
        .as_ref()
        .and_then(|c| c.transport.as_deref())
        .or(manifest.transport.as_deref());
    match declared {
        Some("streamable-http") | Some("http") => "http".to_string(),
        Some("sse") => "sse".to_string(),
        Some("stdio") => "stdio".to_string(),
        _ if source == "remote" => "http".to_string(),
        _ => "stdio".to_string(),
    }
}

/// The catalog's `auth_kind` vocabulary. Prefers the manifest's structured `auth`
/// block and falls back to the coarse `requires` list.
///
/// Only `none` and `api_key` are wired in the activation path today (§15's OAuth /
/// QR / SSH elicitation flow is deferred), so `oauth` here is an honest label on a
/// connector that cannot yet complete its login, not a working mode.
fn norm_auth_kind(entry: &IndexEntry, manifest: &Manifest) -> String {
    if let Some(k) = manifest.auth.as_ref().and_then(|a| a.kind.as_deref()) {
        return match k {
            "oauth2" | "oauth" => "oauth".to_string(),
            "api_key" => "api_key".to_string(),
            "qr" => "qr".to_string(),
            "ssh_key" => "ssh_key".to_string(),
            _ => "none".to_string(),
        };
    }
    let requires: Vec<&str> = manifest
        .requires
        .iter()
        .chain(entry.requires.iter())
        .map(|s| s.as_str())
        .collect();
    if requires.iter().any(|r| r.eq_ignore_ascii_case("oauth")) {
        "oauth".to_string()
    } else if requires.iter().any(|r| r.eq_ignore_ascii_case("api_key")) {
        "api_key".to_string()
    } else {
        "none".to_string()
    }
}

/// The files to install, with their digests. The index is authoritative (it is the
/// document a signature would cover); a manifest-side list is honoured only when
/// the index carries none.
fn files_of<'a>(entry: &'a IndexEntry, manifest: &'a Manifest) -> &'a [FileEntry] {
    if entry.files.is_empty() {
        &manifest.files
    } else {
        &entry.files
    }
}

// ── the card the admin UI renders ─────────────────────────────────────────────

/// One marketplace entry, already translated into Skald's vocabulary so the UI
/// filters on the same words the catalog stores.
#[derive(Debug, Clone, Serialize)]
pub struct MarketplaceCard {
    pub id:                 String,
    pub name:               String,
    pub version:            Option<String>,
    /// `per_user` | `global`
    pub scope:              String,
    /// `remote` | `local_script`
    pub source:             String,
    pub transport:          String,
    pub user_description:   Option<String>,
    pub llm_description:    Option<String>,
    pub requires:           Vec<String>,
    pub tags:               Vec<String>,
    pub homepage:           Option<String>,
    pub auth_kind:          String,
    /// `header` | `query` — how the server wants its key. Shown because a `query`
    /// connector only works via the URL's `{key}` placeholder.
    pub auth_delivery:      Option<String>,
    /// The OAuth scopes this connector will ask each user to grant. The admin
    /// should see the blast radius of a consent before importing it.
    pub oauth_scopes:       Vec<String>,
    pub dependencies:       Vec<String>,
    pub setup_instructions: Vec<String>,
    pub file_count:         usize,
    pub has_icon:           bool,
    /// Already present in `mcp_catalog` under this id.
    pub installed:          bool,
}

fn card_of(h: &Hydrated, installed: bool) -> MarketplaceCard {
    let source = norm_source(&h.entry, &h.manifest);
    let doc = h.manifest.docs.first().cloned().unwrap_or_default();
    MarketplaceCard {
        id:                 h.entry.id.clone(),
        name:               h.entry.name.clone()
                                .or_else(|| h.manifest.name.clone())
                                .unwrap_or_else(|| h.entry.id.clone()),
        version:            h.manifest.version.clone(),
        scope:              norm_scope(&h.entry, &h.manifest),
        transport:          norm_transport(&h.manifest, &source),
        source,
        user_description:   h.entry.user_description.clone().or(doc.description.clone()),
        llm_description:    doc.llm_short_description.clone(),
        requires:           if h.manifest.requires.is_empty() {
                                h.entry.requires.clone()
                            } else {
                                h.manifest.requires.clone()
                            },
        tags:               h.entry.tags.clone(),
        homepage:           h.manifest.homepage.clone(),
        auth_kind:          norm_auth_kind(&h.entry, &h.manifest),
        auth_delivery:      h.manifest.auth.as_ref().and_then(|a| a.delivery.clone()),
        oauth_scopes:       h.manifest.auth.as_ref().map(|a| a.scopes.clone()).unwrap_or_default(),
        dependencies:       h.manifest.dependencies.clone(),
        setup_instructions: h.manifest.setup_instructions.clone(),
        file_count:         files_of(&h.entry, &h.manifest).len(),
        has_icon:           h.entry.icon_small.is_some() || h.entry.icon_large.is_some(),
        installed,
    }
}

// ── fetching + cache ──────────────────────────────────────────────────────────

struct Cache {
    fetched: Instant,
    feed:    Vec<Hydrated>,
}

static CACHE: LazyLock<RwLock<Option<Cache>>> = LazyLock::new(|| RwLock::new(None));

fn folder_of(entry: &IndexEntry) -> String {
    entry.folder.clone().unwrap_or_else(|| entry.id.clone())
}

/// Pulls the index, then every `connector.json` concurrently. The N+1 is only
/// tolerable because the index is small and cached — once the index carries `type`
/// and `scope` for every entry, the listing collapses to a single fetch.
async fn fetch_feed() -> Result<Vec<Hydrated>, ApiError> {
    let base = base_url();
    let index: Index = HTTP
        .get(format!("{base}/connectors.json"))
        .send()
        .await
        .map_err(|e| ApiError::bad_request(format!("cannot reach the marketplace at {base}: {e}")))?
        .error_for_status()
        .map_err(|e| ApiError::bad_request(format!("marketplace returned an error: {e}")))?
        .json()
        .await
        .map_err(|e| ApiError::bad_request(format!("marketplace index is not valid JSON: {e}")))?;

    let mut set = tokio::task::JoinSet::new();
    for entry in index.connectors {
        let base = base.clone();
        set.spawn(async move {
            let url = format!("{}/{}/connector.json", base, folder_of(&entry));
            // A manifest that fails to load degrades that one card to whatever the
            // index said; it never fails the whole listing.
            let manifest = match HTTP.get(&url).send().await {
                Ok(r) => r.json::<Manifest>().await.unwrap_or_default(),
                Err(_) => Manifest::default(),
            };
            Hydrated { entry, manifest }
        });
    }

    let mut feed = Vec::new();
    while let Some(joined) = set.join_next().await {
        if let Ok(h) = joined {
            feed.push(h);
        }
    }
    feed.sort_by(|a, b| a.entry.id.cmp(&b.entry.id));
    Ok(feed)
}

/// The hydrated feed, from cache when warm.
async fn feed(force: bool) -> Result<Vec<Hydrated>, ApiError> {
    if !force {
        if let Some(c) = CACHE.read().await.as_ref() {
            if c.fetched.elapsed() < CACHE_TTL {
                return Ok(c.feed.clone());
            }
        }
    }
    let fresh = fetch_feed().await?;
    *CACHE.write().await = Some(Cache { fetched: Instant::now(), feed: fresh.clone() });
    Ok(fresh)
}

// ── helpers ───────────────────────────────────────────────────────────────────

async fn require_cap(skald: &Skald, user_id: &str, cap: &str) -> Result<(), ApiError> {
    let user = skald_core::db::users::get(skald.db(), user_id)
        .await?
        .ok_or_else(|| ApiError::unauthorized("unknown user"))?;
    if role_capabilities::has(skald.db(), &user.role_id, cap).await? {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!("your role lacks the capability `{cap}`")))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Rejects a feed-supplied path that could escape the connector's own folder.
/// The feed is only semi-trusted (§14) — a hostile or compromised manifest must
/// not be able to name `../../config.yml` and have us write there.
fn safe_rel_path(p: &str) -> Result<&str, ApiError> {
    let bad = p.is_empty()
        || p.starts_with('/')
        || p.contains('\\')
        || p.contains(':')
        || std::path::Path::new(p)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)));
    if bad {
        return Err(ApiError::bad_request(format!(
            "manifest declares an unsafe file path: `{p}`"
        )));
    }
    Ok(p)
}

// ── GET /api/mcp/marketplace ──────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub refresh: bool,
}

/// The whole feed, translated and marked with what is already installed. Search
/// and filtering happen client-side: the list is small, and one payload keeps the
/// UI responsive without a round trip per keystroke.
pub async fn list(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    let feed = feed(q.refresh).await?;
    let installed: std::collections::HashSet<String> = mcp_catalog::list(skald.db())
        .await?
        .into_iter()
        .map(|r| r.name)
        .collect();
    let cards: Vec<MarketplaceCard> = feed
        .iter()
        .map(|h| card_of(h, installed.contains(&h.entry.id)))
        .collect();
    Ok(Json(json!({ "base_url": base_url(), "connectors": cards })))
}

// ── GET /api/mcp/marketplace/{id}/icon ────────────────────────────────────────

#[derive(Deserialize)]
pub struct IconQuery {
    #[serde(default)]
    pub size: Option<String>,
}

/// Proxies a connector icon. Needed because the feed sends no CORS headers, so the
/// page cannot load the image directly.
pub async fn icon(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<String>,
    Query(q): Query<IconQuery>,
) -> Result<Response, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    let feed = feed(false).await?;
    let h = feed
        .iter()
        .find(|h| h.entry.id == id)
        .ok_or_else(|| ApiError::not_found(format!("no marketplace connector `{id}`")))?;

    let large = q.size.as_deref() == Some("lg");
    let rel = if large {
        h.entry.icon_large.clone().or_else(|| h.entry.icon_small.clone())
    } else {
        h.entry.icon_small.clone().or_else(|| h.entry.icon_large.clone())
    }
    .ok_or_else(|| ApiError::not_found("connector declares no icon"))?;

    // The index's icon paths are relative to the feed root, not the folder.
    let url = format!("{}/{}", base_url(), rel.trim_start_matches('/'));
    let res = HTTP
        .get(&url)
        .send()
        .await
        .map_err(|e| ApiError::bad_request(format!("cannot fetch icon: {e}")))?
        .error_for_status()
        .map_err(|e| ApiError::not_found(format!("icon unavailable: {e}")))?;

    let ct = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let bytes = res
        .bytes()
        .await
        .map_err(|e| ApiError::bad_request(format!("cannot read icon: {e}")))?;

    Ok((
        [
            (header::CONTENT_TYPE, ct),
            (header::CACHE_CONTROL, "public, max-age=3600".to_string()),
        ],
        bytes,
    )
        .into_response())
}

// ── POST /api/mcp/marketplace/install ─────────────────────────────────────────

#[derive(Deserialize)]
pub struct InstallBody {
    pub id: String,
}

/// Imports a feed entry into `mcp_catalog` — the act that moves a connector from
/// "someone else vetted this" to "this household's admin accepted it". For an
/// `mcp_local` entry it first downloads and hash-verifies the scripts into
/// `./scripts/<id>/`, which is code landing on the box and therefore needs
/// `mcp.register_local_script` (§14) on top of `mcp.manage_catalog`.
///
/// Installing does **not** activate: a global entry still needs the admin to
/// enable it with a key, a per-user one still needs each user to activate it.
pub async fn install(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<InstallBody>,
) -> Result<Json<Value>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;

    let feed = feed(false).await?;
    let h = feed
        .iter()
        .find(|h| h.entry.id == body.id)
        .ok_or_else(|| ApiError::not_found(format!("no marketplace connector `{}`", body.id)))?;

    let source = norm_source(&h.entry, &h.manifest);
    let scope = norm_scope(&h.entry, &h.manifest);
    let transport = norm_transport(&h.manifest, &source);
    let cfg = h.manifest.mcp_config.clone().unwrap_or_default();
    let doc = h.manifest.docs.first().cloned().unwrap_or_default();

    if source == "local_script" {
        require_cap(&skald, &auth.user_id, role_capabilities::REGISTER_LOCAL_SCRIPT).await?;
    }

    // Download + verify before touching the catalog, so a failed digest leaves no
    // trace of a half-installed connector.
    let (script_path, verified) = if source == "local_script" {
        let files = download_verified(&h.entry, &h.manifest).await?;
        let entry_file = cfg
            .args
            .first()
            .cloned()
            .ok_or_else(|| ApiError::bad_request(
                "manifest has no mcp_config.args[0] naming the script to run",
            ))?;
        let entry_file = safe_rel_path(&entry_file)?.to_string();
        (Some(format!("{}/{}", body.id, entry_file)), files)
    } else {
        (None, 0)
    };

    let args_json = if source == "local_script" {
        // `activate` rewrites args to the in-container path when it copies the
        // script into the user's home, so what is stored here is only a template.
        None
    } else if cfg.args.is_empty() {
        None
    } else {
        serde_json::to_string(&cfg.args).ok()
    };

    // `requires` is the feed's coarse precondition list; the activation UI needs
    // the concrete env keys, which only the manifest's mcp_config knows.
    let config_schema: Vec<String> = cfg.env.keys().cloned().collect();

    let id = mcp_catalog::upsert(
        skald.db(),
        mcp_catalog::UpsertCatalog {
            name:               &h.entry.id,
            scope:              &scope,
            source:             &source,
            transport:          &transport,
            command:            cfg.command.as_deref(),
            args_json,
            env_json:           if cfg.env.is_empty() { None } else { serde_json::to_string(&cfg.env).ok() },
            url:                cfg.url.as_deref(),
            script_path:        script_path.as_deref(),
            config_schema_json: if config_schema.is_empty() { None } else { serde_json::to_string(&config_schema).ok() },
            auth_kind:          &norm_auth_kind(&h.entry, &h.manifest),
            role_filter:        None,
            friendly_name:      h.entry.name.as_deref().or(h.manifest.name.as_deref()),
            // The LLM-facing blurb — this is the column `render_mcp_list` puts in
            // the prompt for `activate_tools()`, so the feed's
            // `llm_short_description` belongs here, not the human `user_description`.
            description:        doc
                                    .llm_short_description
                                    .as_deref()
                                    .or(h.entry.user_description.as_deref()),
        },
    )
    .await?;

    Ok(Json(json!({
        "id":             id,
        "name":           h.entry.id,
        "scope":          scope,
        "source":         source,
        "files_verified": verified,
    })))
}

/// Downloads every file the manifest declares into `./scripts/<id>/`, refusing any
/// whose SHA-256 does not match. All-or-nothing: files are verified in memory and
/// only written once every digest checks out, so a tampered feed never leaves a
/// partial connector on disk. Returns how many files were verified.
async fn download_verified(entry: &IndexEntry, manifest: &Manifest) -> Result<usize, ApiError> {
    let files = files_of(entry, manifest);
    if files.is_empty() {
        return Err(ApiError::bad_request(
            "the feed declares no `files` with digests for this connector — \
             refusing to install unverifiable code (§14)",
        ));
    }

    let base = base_url();
    let folder = folder_of(entry);
    let mut staged: Vec<(String, Vec<u8>)> = Vec::new();

    for f in files {
        let rel = safe_rel_path(&f.path)?;

        // Defensive: a document can never carry its own digest (writing the hash
        // changes the file), so a self-entry is unverifiable by construction. The
        // feed now keeps digests in the index, where this cannot arise.
        if rel == "connector.json" {
            continue;
        }

        if let Some(sz) = f.size {
            if sz > MAX_FILE_BYTES {
                return Err(ApiError::bad_request(format!(
                    "`{rel}` declares {sz} bytes, over the {MAX_FILE_BYTES} limit"
                )));
            }
        }

        let url = format!("{base}/{folder}/{rel}");
        let bytes = HTTP
            .get(&url)
            .send()
            .await
            .map_err(|e| ApiError::bad_request(format!("cannot download `{rel}`: {e}")))?
            .error_for_status()
            .map_err(|e| ApiError::bad_request(format!("cannot download `{rel}`: {e}")))?
            .bytes()
            .await
            .map_err(|e| ApiError::bad_request(format!("cannot read `{rel}`: {e}")))?;

        let got = sha256_hex(&bytes);
        if !got.eq_ignore_ascii_case(f.sha256.trim()) {
            return Err(ApiError::bad_request(format!(
                "digest mismatch on `{rel}`: the manifest declares {} but the served \
                 file hashes to {got}. Refusing to install.",
                f.sha256
            )));
        }
        staged.push((rel.to_string(), bytes.to_vec()));
    }

    if staged.is_empty() {
        return Err(ApiError::bad_request(
            "manifest declares no installable file besides connector.json",
        ));
    }

    let wd = std::env::current_dir()
        .map_err(|e| ApiError::bad_request(format!("cannot resolve working directory: {e}")))?;
    let dest = wd.join("scripts").join(&entry.id);
    std::fs::create_dir_all(&dest)
        .map_err(|e| ApiError::bad_request(format!("cannot create {}: {e}", dest.display())))?;

    let count = staged.len();
    for (rel, bytes) in staged {
        let path = dest.join(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ApiError::bad_request(format!("cannot create dir for `{rel}`: {e}")))?;
        }
        std::fs::write(&path, &bytes)
            .map_err(|e| ApiError::bad_request(format!("cannot write `{rel}`: {e}")))?;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(json: &str) -> IndexEntry {
        serde_json::from_str(json).expect("index entry")
    }
    fn manifest(json: &str) -> Manifest {
        serde_json::from_str(json).expect("manifest")
    }

    /// A feed-supplied path must never escape the connector's own folder. The feed
    /// is only semi-trusted (§14) — this is what stops a hostile manifest naming
    /// `../../config.yml`.
    #[test]
    fn rejects_path_traversal_from_the_feed() {
        for bad in [
            "../../config.yml",
            "/etc/passwd",
            "a/../../b",
            "..",
            "",
            "C:\\evil",
            "dir\\file.py",
        ] {
            assert!(safe_rel_path(bad).is_err(), "should have rejected `{bad}`");
        }
        for ok in ["server.py", "pkg/server.py", "requirements.txt"] {
            assert!(safe_rel_path(ok).is_ok(), "should have accepted `{ok}`");
        }
    }

    /// Placement (§7) is not transport: the feed's explicit `scope` decides, and a
    /// feed that says nothing must fall to the narrower blast radius.
    #[test]
    fn scope_comes_from_the_feed_not_from_the_transport() {
        assert_eq!(
            norm_scope(&entry(r#"{"id":"t","scope":"global"}"#), &Manifest::default()),
            "global"
        );
        assert_eq!(
            norm_scope(&entry(r#"{"id":"g","scope":"user"}"#), &Manifest::default()),
            "per_user"
        );
        // A *remote* connector explicitly scoped per-user stays per-user — this is
        // the case `mcp.register_remote` exists for, and the one an infer-from-
        // transport shortcut would get wrong.
        assert_eq!(
            norm_scope(
                &entry(r#"{"id":"r","scope":"user","type":"mcp_remote"}"#),
                &manifest(r#"{"type":"mcp_remote"}"#)
            ),
            "per_user"
        );
        // Silence → the narrower answer.
        assert_eq!(norm_scope(&entry(r#"{"id":"x"}"#), &Manifest::default()), "per_user");
    }

    /// An unreadable `type` must resolve to the answer that demands MORE authority,
    /// so a misread can never under-gate an install past §14's admin-only check.
    #[test]
    fn unknown_source_fails_closed_to_local_script() {
        assert_eq!(norm_source(&entry(r#"{"id":"x"}"#), &Manifest::default()), "local_script");
        assert_eq!(
            norm_source(&entry(r#"{"id":"x","type":"mcp_remote"}"#), &Manifest::default()),
            "remote"
        );
        assert_eq!(
            norm_source(&entry(r#"{"id":"x","type":"mcp_local"}"#), &Manifest::default()),
            "local_script"
        );
    }

    /// `transport_of` in skald-core maps anything unknown to Stdio, so an unmapped
    /// `streamable-http` would silently try to spawn a command instead of making an
    /// HTTP call.
    #[test]
    fn streamable_http_normalises_to_http() {
        let m = manifest(r#"{"mcp_config":{"transport":"streamable-http","url":"https://x/"}}"#);
        assert_eq!(norm_transport(&m, "remote"), "http");
        // A remote entry that names no transport still must not become stdio.
        assert_eq!(norm_transport(&Manifest::default(), "remote"), "http");
        assert_eq!(norm_transport(&Manifest::default(), "local_script"), "stdio");
    }

    /// The structured `auth` block wins over the coarse `requires` list.
    #[test]
    fn auth_kind_prefers_the_structured_block() {
        let m = manifest(r#"{"auth":{"type":"oauth2","scopes":["a","b"]},"requires":["API_KEY"]}"#);
        assert_eq!(norm_auth_kind(&entry(r#"{"id":"x"}"#), &m), "oauth");
        let m = manifest(r#"{"auth":{"type":"api_key","delivery":"query","param":"k"}}"#);
        assert_eq!(norm_auth_kind(&entry(r#"{"id":"x"}"#), &m), "api_key");
        // No auth block → fall back to `requires`.
        assert_eq!(
            norm_auth_kind(&entry(r#"{"id":"x","requires":["API_KEY"]}"#), &Manifest::default()),
            "api_key"
        );
    }

    /// Digests live in the index now; a manifest-side list is only a fallback.
    #[test]
    fn index_digests_win_over_manifest_digests() {
        let e = entry(r#"{"id":"x","files":[{"path":"a.py","sha256":"aa"}]}"#);
        let m = manifest(r#"{"files":[{"path":"b.py","sha256":"bb"}]}"#);
        assert_eq!(files_of(&e, &m).len(), 1);
        assert_eq!(files_of(&e, &m)[0].path, "a.py");
        assert_eq!(files_of(&entry(r#"{"id":"x"}"#), &m)[0].path, "b.py");
    }

    #[test]
    fn sha256_matches_a_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// Hits the real feed. `#[ignore]`d so the suite stays offline-clean; run with
    /// `cargo test --bin skald -- --ignored live_feed`.
    #[tokio::test]
    #[ignore]
    async fn live_feed_parses_and_verifies() {
        // `main()` installs the process-wide rustls provider before any handshake;
        // under the test harness main never runs, so do it here. Ignore the error:
        // another test may have installed it already.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let feed = match fetch_feed().await {
            Ok(f) => f,
            Err(e) => panic!("feed unreachable: {}", e.message),
        };
        assert!(!feed.is_empty(), "feed returned no connectors");

        for h in &feed {
            let c = card_of(h, false);
            println!(
                "{:<8} scope={:<8} source={:<12} transport={:<6} auth={:<7} files={}",
                c.id, c.scope, c.source, c.transport, c.auth_kind, c.file_count
            );
            assert!(matches!(c.scope.as_str(), "per_user" | "global"));
            assert!(matches!(c.source.as_str(), "remote" | "local_script"));
            // Anything that is not stdio must have been normalised into a value
            // `transport_of` actually understands.
            assert!(matches!(c.transport.as_str(), "stdio" | "http" | "sse"));
            assert!(
                c.llm_description.is_some(),
                "`{}` has no llm_short_description — the agent would see nothing \
                 when deciding whether to activate_tools() on it",
                c.id
            );
        }

        // Every declared digest must match what the site actually serves.
        for h in &feed {
            for f in files_of(&h.entry, &h.manifest) {
                let url = format!("{}/{}/{}", base_url(), folder_of(&h.entry), f.path);
                let bytes = HTTP.get(&url).send().await.unwrap().bytes().await.unwrap();
                assert_eq!(
                    sha256_hex(&bytes).to_lowercase(),
                    f.sha256.trim().to_lowercase(),
                    "digest mismatch for {}/{}",
                    h.entry.id,
                    f.path
                );
            }
        }
    }
}
