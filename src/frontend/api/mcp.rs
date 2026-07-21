//! Connectors (MCP) management API (blueprint §14/§15).
//!
//! Two audiences, capability-gated (`role_capabilities`):
//! - **Admin** curates the catalog (`mcp_catalog`) and enables globally-active
//!   connectors (`mcp_global_servers` + `mcp_global_access`).
//! - **Any user** activates per-user connectors from the catalog into their own
//!   `{userid}.db` (`mcp_user_servers`), started inside their container.
//!
//! Registration is UI/API-driven, never agent-driven — the prompt-injection→
//! local-script→RCE path (§14) is gone with the old `register_mcp` tool.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use skald_core::db::{mcp_catalog, mcp_catalog_access, mcp_global_access, mcp_global_servers, mcp_user_servers, oauth_providers, role_capabilities};
use skald_core::skald::Skald;

use super::caps::require_cap;
use super::guard::AuthUser;
use super::{require_context, ApiError};

// ── helpers ───────────────────────────────────────────────────────────────────

fn to_json_opt<T: serde::Serialize>(v: &Option<T>) -> Option<String> {
    v.as_ref().and_then(|x| serde_json::to_string(x).ok())
}

/// Installs the connector folder that `script_path` (`<connector>/<file>`) belongs
/// to into the caller's container home, and returns the path the entry file will
/// have INSIDE the container.
///
/// The whole folder travels, not just the entry file — which is what finally gets a
/// connector's `requirements.txt` and its multi-file trees to where the server
/// actually runs. The home is the only durable zone (§6), so it survives a
/// container recreate.
fn install_connector_for_user(
    user_id:     &str,
    name:        &str,
    script_path: &str,
) -> Result<String, ApiError> {
    let (folder, entry_file) = skald_core::mcp::split_script_path(script_path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let dir = skald_core::mcp::install_into_home(user_id, name, folder)
        .map_err(|e| ApiError::bad_request(format!("failed to install connector files: {e}")))?
        .ok_or_else(|| ApiError::bad_request(format!(
            "connector `{folder}` has no installed files under ./{}/ — \
             reinstall it from the marketplace",
            skald_core::mcp::CONNECTORS_DIR,
        )))?;
    Ok(dir.join(entry_file).to_string_lossy().into_owned())
}

// ── verify-before-save helpers ───────────────────────────────────────────────

/// One entry of the catalog's `config_schema_json` (the marketplace `env[]`).
/// Drives both the activation form (frontend) and the env/secret split (here).
#[derive(Debug, serde::Deserialize)]
struct EnvSchemaEntry {
    name:        String,
    #[serde(default)] secret: bool,
    #[allow(dead_code)]
    #[serde(default)] required: bool,
}

/// Parses the catalog's `config_schema_json` into schema entries. Tolerates the
/// legacy `["KEY1","KEY2"]` shape (treated as non-secret) and the new object
/// array; anything unparseable yields an empty schema.
fn parse_env_schema(config_schema_json: &Option<String>) -> Vec<EnvSchemaEntry> {
    let raw = match config_schema_json.as_deref() {
        Some(s) => s,
        None => return Vec::new(),
    };
    // Object-array form (the new manifest `env[]`).
    if let Ok(v) = serde_json::from_str::<Vec<EnvSchemaEntry>>(raw) {
        return v;
    }
    // Legacy bare-name form.
    serde_json::from_str::<Vec<String>>(raw)
        .unwrap_or_default()
        .into_iter()
        .map(|name| EnvSchemaEntry { name, secret: false, required: false })
        .collect()
}

/// Splits the form values into non-secret (`env`) and secret (`secret`) maps,
/// the two channels [`run_verify`] substitutes into `{ENV:…}` / `{SECRET:…}`.
///
/// When `auth_kind == "api_key"`, the supplied `api_key` is also injected under
/// every secret-name declared by the schema — the canonical case being Tavily,
/// whose schema names the key `tavilyApiKey` and whose URL carries the matching
/// `{SECRET:tavilyApiKey}` token.
fn split_form_values(
    form: Option<&HashMap<String, String>>,
    api_key: Option<&str>,
    schema: &[EnvSchemaEntry],
    auth_kind: &str,
) -> (HashMap<String, String>, HashMap<String, String>) {
    let secret_names: std::collections::HashSet<&str> =
        schema.iter().filter(|e| e.secret).map(|e| e.name.as_str()).collect();

    let mut env = HashMap::new();
    let mut secret = HashMap::new();
    if let Some(form) = form {
        for (k, v) in form {
            if secret_names.contains(k.as_str()) {
                secret.insert(k.clone(), v.clone());
            } else {
                env.insert(k.clone(), v.clone());
            }
        }
    }

    // An api_key connector maps the key into every declared secret name that the
    // form did not already fill (the UI's api_key box is the same value).
    if auth_kind == "api_key" {
        if let Some(key) = api_key {
            for name in secret_names {
                secret.entry(name.to_string()).or_insert_with(|| key.to_string());
            }
        }
    }
    (env, secret)
}

/// Installs the connector folder holding a catalog entry's verify script into the
/// user's home, returning the in-container directory to run it from. `None` if the
/// entry declares no verify script.
///
/// This runs on the Test button too, before any activation exists — which is why it
/// installs rather than assuming the folder is already there.
fn prepare_user_verify_workdir(
    user_id: &str,
    name: &str,
    entry: &mcp_catalog::McpCatalogRow,
) -> Result<Option<std::path::PathBuf>, ApiError> {
    let verify_path = match &entry.verify_script_path {
        Some(p) => p,
        None => return Ok(None),
    };
    let (folder, _) = skald_core::mcp::split_script_path(verify_path)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let dir = skald_core::mcp::install_into_home(user_id, name, folder)
        .map_err(|e| ApiError::bad_request(format!("failed to install connector files: {e}")))?
        .ok_or_else(|| ApiError::bad_request(format!(
            "connector `{folder}` has no installed files — reinstall it from the marketplace"
        )))?;
    Ok(Some(dir))
}

/// The host working dir for a global connector's verify script — the
/// `./connectors/<catalog_name>/` directory the marketplace installer populated.
fn global_verify_workdir(catalog_name: &str) -> Result<std::path::PathBuf, ApiError> {
    let dir = skald_core::mcp::connector_dir(catalog_name)
        .map_err(|e| ApiError::bad_request(format!("cannot resolve the connectors dir: {e}")))?;
    if !dir.is_dir() {
        return Err(ApiError::bad_request(format!(
            "connector directory `{}/{catalog_name}` not found — reinstall the connector",
            skald_core::mcp::CONNECTORS_DIR,
        )));
    }
    Ok(dir)
}

/// Default timeout for a connector-declared verify step. (The manifest can also
/// carry `verify.timeout_secs`; wiring that through is a follow-up.)
const VERIFY_TIMEOUT_SECS: u64 = 20;

// ── connector icons ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct IconQuery {
    /// `sm` (default) | `lg`.
    #[serde(default)]
    pub size: Option<String>,
}

/// `GET /api/mcp/catalog/{name}/icon?size=sm|lg` — the icon of an **installed**
/// connector, served off `./connectors/<name>/`.
///
/// Authenticated but deliberately **not** capability-gated: seeing the icon of a
/// connector you are allowed to activate is not an administrative act. The
/// marketplace's own icon endpoint cannot do this job — it proxies the live feed
/// behind `manage_catalog`, so a normal user gets a 403, and the image would vanish
/// the moment the feed went down or the entry was pulled upstream. Once installed,
/// the bytes are ours.
pub async fn catalog_icon(
    State(skald): State<Arc<Skald>>,
    Extension(_auth): Extension<AuthUser>,
    Path(name): Path<String>,
    axum::extract::Query(q): axum::extract::Query<IconQuery>,
) -> Result<axum::response::Response, ApiError> {
    use axum::http::header;
    use axum::response::IntoResponse;

    let entry = mcp_catalog::get_by_name(skald.db(), &name).await?
        .ok_or_else(|| ApiError::not_found(format!("no catalog entry `{name}`")))?;

    let large = q.size.as_deref() == Some("lg");
    let rel = if large {
        entry.icon_large_path.clone().or_else(|| entry.icon_small_path.clone())
    } else {
        entry.icon_small_path.clone().or_else(|| entry.icon_large_path.clone())
    }
    .ok_or_else(|| ApiError::not_found("this connector has no installed icon"))?;

    // Containment, mirroring `tools::fs::resolve_host_path`: canonicalize and
    // prefix-check, fail-closed. `rel` only ever holds a path the installer already
    // proved safe, and `name` only ever names a real catalog row — this is the belt
    // to those braces, and the reason a bad row cannot turn into an arbitrary read.
    let dir = skald_core::mcp::connector_dir(&entry.name)
        .map_err(|e| ApiError::bad_request(format!("cannot resolve the connectors dir: {e}")))?;
    let base = dir.canonicalize()
        .map_err(|_| ApiError::not_found("this connector has no installed files"))?;
    let path = base.join(&rel).canonicalize()
        .map_err(|_| ApiError::not_found("icon file is missing — reinstall the connector"))?;
    if !path.starts_with(&base) {
        return Err(ApiError::forbidden("icon path escapes the connector directory"));
    }

    let bytes = std::fs::read(&path)
        .map_err(|e| ApiError::bad_request(format!("cannot read icon: {e}")))?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    let ct = skald_core::mcp::content_type_for_ext(&ext);

    // An icon is untrusted bytes from a remote feed, and half of them are SVGs —
    // a format that can carry script. Served from our own origin, that would be
    // stored XSS for anyone who opened the URL directly. `nosniff` pins the type,
    // and the CSP neuters any script or fetch the document tries to perform.
    Ok((
        [
            (header::CONTENT_TYPE, ct.to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'; style-src 'unsafe-inline'".to_string()),
            (header::CACHE_CONTROL, "private, max-age=3600".to_string()),
        ],
        bytes,
    )
        .into_response())
}

// ── existing: running-server introspection ────────────────────────────────────

/// The globally-running MCP servers and their tools (host runtime).
pub async fn list_servers(State(skald): State<Arc<Skald>>) -> Json<Vec<Value>> {
    Json(skald.mcp().server_infos())
}

// ── admin: catalog CRUD ───────────────────────────────────────────────────────

pub async fn catalog_list(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<mcp_catalog::McpCatalogRow>>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    Ok(Json(mcp_catalog::list(skald.db()).await?))
}

#[derive(Deserialize)]
pub struct CatalogUpsertBody {
    pub name:          String,
    pub scope:         String,               // 'per_user' | 'global'
    pub source:        String,               // 'remote' | 'local_script'
    #[serde(default = "default_stdio")]
    pub transport:     String,
    pub command:       Option<String>,
    pub args:          Option<Vec<String>>,
    pub env:           Option<HashMap<String, String>>,
    pub url:           Option<String>,
    pub script_path:   Option<String>,
    pub config_schema: Option<Vec<String>>,
    #[serde(default = "default_none_auth")]
    pub auth_kind:     String,
    pub role_filter:   Option<Vec<String>>,
    pub verify_command:     Option<String>,
    pub verify_script_path: Option<String>,
    pub friendly_name: Option<String>,
    pub description:   Option<String>,
}

fn default_stdio() -> String { "stdio".into() }
fn default_none_auth() -> String { "none".into() }

pub async fn catalog_upsert(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<CatalogUpsertBody>,
) -> Result<Json<Value>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    // Adding a NEW local script to the catalog is the RCE-bearing act (§14): it
    // needs the admin-only capability on top of catalog management.
    if body.source == "local_script" {
        require_cap(&skald, &auth.user_id, role_capabilities::REGISTER_LOCAL_SCRIPT).await?;
    }
    let id = mcp_catalog::upsert(skald.db(), mcp_catalog::UpsertCatalog {
        name:               &body.name,
        scope:              &body.scope,
        source:             &body.source,
        transport:          &body.transport,
        command:            body.command.as_deref(),
        args_json:          to_json_opt(&body.args),
        env_json:           to_json_opt(&body.env),
        url:                body.url.as_deref(),
        script_path:        body.script_path.as_deref(),
        config_schema_json: to_json_opt(&body.config_schema),
        auth_kind:          &body.auth_kind,
        // OAuth catalog entries come from the vetted feed (marketplace install),
        // not the admin's manual form — so these stay unset here.
        oauth_provider:     None,
        oauth_scopes_json:  None,
        deliver_json:       None,
        role_filter:        to_json_opt(&body.role_filter),
        verify_command:     body.verify_command.as_deref(),
        verify_script_path: body.verify_script_path.as_deref(),
        // Not the admin form's to set: the installer owns them, and `upsert`
        // COALESCEs these away rather than blanking an installed connector's icons.
        icon_small_path:    None,
        icon_large_path:    None,
        friendly_name:      body.friendly_name.as_deref(),
        description:        body.description.as_deref(),
        // Versioning is the feed's to set (marketplace install); the manual form
        // leaves it untouched (COALESCE in `upsert`).
        version:                None,
        version_string:         None,
        version_release_date:   None,
    }).await?;
    Ok(Json(json!({ "id": id })))
}

pub async fn catalog_delete(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    mcp_catalog::delete(skald.db(), id).await?;
    Ok(Json(json!({ "ok": true })))
}

// ── admin: OAuth providers (§15) ──────────────────────────────────────────────

/// The OAuth identity providers, **without** client secrets (never leaves the
/// process for the browser).
pub async fn providers_list(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<oauth_providers::OauthProviderView>>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    let rows = oauth_providers::list(skald.db()).await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

#[derive(Deserialize)]
pub struct ProviderUpsertBody {
    pub name:          String,
    pub display_name:  String,
    pub auth_url:      String,
    pub token_url:     String,
    pub client_id:     String,
    /// Empty keeps the stored secret — the list view never gave it back, so editing
    /// the URLs must not force the admin to re-paste it.
    #[serde(default)]
    pub client_secret: String,
    pub redirect_uri:  String,
    pub extra_params:  Option<String>,
}

pub async fn providers_upsert(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<ProviderUpsertBody>,
) -> Result<Json<Value>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    // `extra_params` must be valid JSON — it is merged into the consent URL, and a
    // malformed value would silently drop `access_type`/`prompt` and cost the user a
    // refresh token. Reject it here rather than fail quietly at sign-in.
    if let Some(extra) = body.extra_params.as_deref().filter(|s| !s.trim().is_empty()) {
        serde_json::from_str::<HashMap<String, String>>(extra)
            .map_err(|e| ApiError::bad_request(format!("extra_params is not a JSON object of strings: {e}")))?;
    }
    let extra = body.extra_params.as_deref().filter(|s| !s.trim().is_empty());
    oauth_providers::upsert(skald.db(), oauth_providers::UpsertProvider {
        name:          &body.name,
        display_name:  &body.display_name,
        auth_url:      &body.auth_url,
        token_url:     &body.token_url,
        client_id:     &body.client_id,
        client_secret: &body.client_secret,
        redirect_uri:  &body.redirect_uri,
        extra_params:  extra,
    }).await?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn providers_delete(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(name): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    oauth_providers::delete(skald.db(), &name).await?;
    Ok(Json(json!({ "ok": true })))
}

// ── admin: globally-active connectors + access ────────────────────────────────

pub async fn global_list(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<mcp_global_servers::McpGlobalServerRow>>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    Ok(Json(mcp_global_servers::all(skald.db()).await?))
}

#[derive(Deserialize)]
pub struct GlobalEnableBody {
    /// The catalog entry to enable globally (must be scope='global').
    pub catalog_name:  String,
    /// Optional runtime name override (defaults to the catalog name).
    pub name:          Option<String>,
    pub api_key:       Option<String>,
    pub env:           Option<HashMap<String, String>>,
}

pub async fn global_enable(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<GlobalEnableBody>,
) -> Result<Json<Value>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    let entry = mcp_catalog::get_by_name(skald.db(), &body.catalog_name).await?
        .ok_or_else(|| ApiError::not_found(format!("no catalog entry `{}`", body.catalog_name)))?;
    if entry.scope != "global" {
        return Err(ApiError::bad_request("catalog entry is not a global connector"));
    }
    let name = body.name.clone().unwrap_or_else(|| entry.name.clone());

    // A `global` `local_script` runs on the HOST, straight out of `connectors/<id>/`.
    // Unlike a per-user activation — whose args `activate` rewrites to the in-container
    // script path — nothing else sets its entry-script path (the catalog nulls
    // `args_json` for a local_script, deferring the rewrite to `activate`, which a
    // global connector never reaches). Resolve it to the host-absolute path here, or
    // the launch would be a bare `python3`: an stdin REPL that never answers
    // `initialize` and times out after 120s. Also install its declared deps beside the
    // script (`.pydeps`), since the global runtime has no container reconciler; the
    // matching `PYTHONPATH` is set by `global_row_spec`.
    let args_json = if entry.source == "local_script" {
        let script_path = entry.script_path.as_deref().ok_or_else(|| {
            ApiError::bad_request("catalog local_script entry has no script_path")
        })?;
        let (folder, rel) = skald_core::mcp::split_script_path(script_path)
            .map_err(|e| ApiError::bad_request(e.to_string()))?;
        skald_core::mcp::ensure_installed_host(folder)
            .await
            .map_err(|e| ApiError::bad_request(format!("dependency install failed: {e}")))?;
        let abs = skald_core::mcp::connector_dir(folder)
            .map_err(|e| ApiError::bad_request(e.to_string()))?
            .join(rel);
        Some(serde_json::to_string(&[abs.to_string_lossy().into_owned()])
            .map_err(|e| ApiError::bad_request(e.to_string()))?)
    } else {
        entry.args_json.clone()
    };

    // Snapshot the concrete config from the catalog; the admin supplies the secret.
    let id = mcp_global_servers::upsert(skald.db(), mcp_global_servers::UpsertGlobal {
        name:               &name,
        catalog_name:       Some(&entry.name),
        transport:          &entry.transport,
        command:            entry.command.as_deref(),
        args_json,
        env_json:           body.env.as_ref().and_then(|e| serde_json::to_string(e).ok()).or_else(|| entry.env_json.clone()),
        url:                entry.url.as_deref(),
        api_key:            body.api_key.as_deref(),
        verify_command:     entry.verify_command.as_deref(),
        verify_script_path: entry.verify_script_path.as_deref(),
        friendly_name:      entry.friendly_name.as_deref(),
        description:        entry.description.as_deref(),
    }).await?;

    // Verify the admin-supplied credentials before starting the server. A failure
    // disables the row so it does not run with bad creds; the admin sees the
    // message and can fix + re-enable. A connector with no verify step is allowed
    // through unchanged.
    let verify = run_verify_for_entry(&skald, &auth, &entry, &name, body.env.as_ref(), body.api_key.as_deref()).await?;
    if !verify.skipped && !verify.ok {
        mcp_global_servers::set_enabled(skald.db(), id, false).await?;
        return Ok(Json(json!({ "id": id, "verify": verify, "error": verify.message })));
    }

    // Start it now in the global runtime (host transport).
    let row = mcp_global_servers::get(skald.db(), id).await?
        .ok_or_else(|| ApiError::bad_request("global server vanished after upsert"))?;
    let spec = skald_core::mcp::global_row_spec(&row);
    match skald.mcp().start_server(spec).await {
        Ok(tools) => Ok(Json(json!({ "id": id, "tools": tools, "verify": verify }))),
        Err(e)    => Ok(Json(json!({ "id": id, "error": e.to_string(), "verify": verify }))),
    }
}

pub async fn global_delete(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    if let Some(row) = mcp_global_servers::get(skald.db(), id).await? {
        skald.mcp().stop_server(&row.name);
    }
    mcp_global_servers::delete(skald.db(), id).await?;
    Ok(Json(json!({ "ok": true })))
}

/// Runs a catalog entry's verify step in the right target — inside the caller's
/// container for a `per_user` local_script, on the host for a `global` remote.
/// Returns [`VerifyReport::skipped`] when the entry declares no verify step, so
/// callers can treat "no test" uniformly.
///
/// `runtime_name` is the in-container directory key (the user's chosen runtime
/// name for an activation, or the catalog name for a standalone Test).
async fn run_verify_for_entry(
    skald: &Skald,
    auth: &AuthUser,
    entry: &mcp_catalog::McpCatalogRow,
    runtime_name: &str,
    form: Option<&HashMap<String, String>>,
    api_key: Option<&str>,
) -> Result<skald_core::mcp::VerifyReport, ApiError> {
    use skald_core::mcp::{run_verify, VerifyReport, VerifyTarget};

    let verify_command = match entry.verify_command.as_deref() {
        Some(c) => c,
        None => return Ok(VerifyReport::skipped()),
    };
    let schema = parse_env_schema(&entry.config_schema_json);
    let (env_values, secret_values) = split_form_values(form, api_key, &schema, &entry.auth_kind);

    let report = match entry.scope.as_str() {
        "per_user" => {
            // `require_context` ensures the container exists before we exec into it.
            let _ctx = require_context(skald, &auth.user_id).await?;
            let workdir = prepare_user_verify_workdir(&auth.user_id, runtime_name, entry)?
                .ok_or_else(|| ApiError::bad_request(
                    "catalog entry declares verify_command but no verify_script_path",
                ))?;
            let container = skald_core::container::container_name(&auth.user_id);
            run_verify(
                verify_command,
                &env_values,
                &secret_values,
                VerifyTarget::Container { container: &container, workdir: &workdir },
                VERIFY_TIMEOUT_SECS,
            ).await
        }
        "global" => {
            require_cap(skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
            let workdir = global_verify_workdir(&entry.name)?;
            run_verify(
                verify_command,
                &env_values,
                &secret_values,
                VerifyTarget::Host { workdir: &workdir },
                VERIFY_TIMEOUT_SECS,
            ).await
        }
        other => return Err(ApiError::bad_request(format!("unknown catalog scope `{other}`"))),
    };
    Ok(report)
}

// ── user: test a connector without persisting ─────────────────────────────────

#[derive(Deserialize)]
pub struct TestBody {
    /// The catalog entry whose credentials to probe.
    pub catalog_name: String,
    /// The form values the user just typed (env + secret mixed).
    pub env:          Option<HashMap<String, String>>,
    pub api_key:      Option<String>,
}

/// `POST /api/mcp/test` — runs a connector's verify step with the supplied
/// credentials and returns the [`VerifyReport`] **without persisting anything**.
/// The frontend Test button calls this before offering Activate.
pub async fn test(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<TestBody>,
) -> Result<Json<skald_core::mcp::VerifyReport>, ApiError> {
    let entry = mcp_catalog::get_by_name(skald.db(), &body.catalog_name).await?
        .ok_or_else(|| ApiError::not_found(format!("no catalog entry `{}`", body.catalog_name)))?;
    // Per-role gate, mirroring `activate`: a user may only test connectors
    // their role is allowed to activate.
    let user = skald_core::db::users::get(skald.db(), &auth.user_id).await?
        .ok_or_else(|| ApiError::unauthorized("unknown user"))?;
    if !entry.allowed_for_role(&user.role_id) {
        return Err(ApiError::forbidden("your role may not use this connector"));
    }
    let report = run_verify_for_entry(
        &skald, &auth, &entry, &entry.name,
        body.env.as_ref(), body.api_key.as_deref(),
    ).await?;
    Ok(Json(report))
}

#[derive(Deserialize)]
pub struct GlobalAccessBody {
    /// The full set of user ids allowed to use this global connector.
    pub user_ids: Vec<String>,
}

pub async fn global_get_access(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<String>>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    Ok(Json(mcp_global_access::users_for_server(skald.db(), id).await?))
}

pub async fn global_set_access(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<i64>,
    Json(body): Json<GlobalAccessBody>,
) -> Result<Json<Value>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    mcp_global_access::set_access(skald.db(), id, &body.user_ids).await?;
    Ok(Json(json!({ "ok": true })))
}

// ── admin: per-user connector access (the Users-page "who can use what") ───────
//
// One surface over both access tables: which registered connectors the admin has
// authorized for a given user. `global` rows write `mcp_global_access`, `catalog`
// rows write `mcp_catalog_access`. For a global the grant is immediate access; for
// a catalog entry it is *eligibility to activate* — the user still supplies their
// own credentials / OAuth in their own Connectors page. Admin-only.

/// One registered connector as the Users-page access checklist renders it.
#[derive(serde::Serialize)]
pub struct UserConnectorView {
    /// `"global"` | `"catalog"` — which access table `name`/`id` belongs to.
    pub kind:          &'static str,
    /// Global server id (the `mcp_global_access` key); `None` for catalog rows.
    pub id:            Option<i64>,
    /// Global runtime name OR catalog entry name — the grant key for its table.
    pub name:          String,
    pub friendly_name: Option<String>,
    pub description:   Option<String>,
    /// Global only: a disabled global is nobody's to use yet (shown greyed).
    pub enabled:       bool,
    /// Whether this user is currently authorized for it.
    pub granted:       bool,
}

/// `GET /api/users/{id}/connectors` — every registered connector with this user's
/// grant flag. Admin-only.
pub async fn user_connectors_get(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(target): Path<String>,
) -> Result<Json<Vec<UserConnectorView>>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    skald_core::db::users::get(skald.db(), &target).await?
        .ok_or_else(|| ApiError::not_found("no such user"))?;

    let granted_catalog: std::collections::HashSet<String> =
        mcp_catalog_access::catalog_names_for_user(skald.db(), &target).await?
            .into_iter().collect();

    let mut out: Vec<UserConnectorView> = Vec::new();
    for s in mcp_global_servers::all(skald.db()).await? {
        let granted = mcp_global_access::has_access(skald.db(), s.id, &target).await?;
        out.push(UserConnectorView {
            kind: "global", id: Some(s.id), name: s.name,
            friendly_name: s.friendly_name, description: s.description,
            enabled: s.enabled, granted,
        });
    }
    for e in mcp_catalog::list_for_scope(skald.db(), "per_user").await? {
        let granted = granted_catalog.contains(&e.name);
        out.push(UserConnectorView {
            kind: "catalog", id: None, name: e.name.clone(),
            friendly_name: e.friendly_name, description: e.description,
            enabled: true, granted,
        });
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct UserConnectorsBody {
    #[serde(default)]
    pub global_ids:    Vec<i64>,
    #[serde(default)]
    pub catalog_names: Vec<String>,
}

/// `PUT /api/users/{id}/connectors` — replaces this user's full access set across
/// both tables. Admin-only.
pub async fn user_connectors_set(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(target): Path<String>,
    Json(body): Json<UserConnectorsBody>,
) -> Result<Json<Value>, ApiError> {
    require_cap(&skald, &auth.user_id, role_capabilities::MANAGE_CATALOG).await?;
    skald_core::db::users::get(skald.db(), &target).await?
        .ok_or_else(|| ApiError::not_found("no such user"))?;

    // Globals settle at the target user's next login (their `accessible_global`
    // snapshot is captured then) — same as the existing per-server access flow.
    mcp_global_access::set_for_user(skald.db(), &target, &body.global_ids).await?;

    // Catalog: apply the grant set; `set_for_user` returns the names this revoked.
    let revoked = mcp_catalog_access::set_for_user(skald.db(), &target, &body.catalog_names).await?;

    // Immediate revoke for a LIVE user: stop + drop any now-forbidden activation.
    // A locked user cannot be reached (their DB is sealed to the admin); the
    // startup access filter keeps the connector dormant from their next login on.
    if !revoked.is_empty() {
        if let Some(ctx) = skald.user_context_if_live(&target).await {
            if let Ok(rows) = mcp_user_servers::all(&ctx.pool).await {
                for r in rows {
                    if r.catalog_name.as_deref().is_some_and(|c| revoked.iter().any(|n| n == c)) {
                        ctx.user_mcp.stop_server(&r.name);
                        let _ = mcp_user_servers::delete(&ctx.pool, r.id).await;
                    }
                }
            }
        }
    }
    Ok(Json(json!({ "ok": true })))
}

// ── user: available catalog + activation ──────────────────────────────────────

/// A globally-active connector as the Connectors page renders it.
///
/// Deliberately **not** [`mcp_global_servers::McpGlobalServerRow`]: that row carries
/// `api_key`, and this view reaches every logged-in user, not just the admin. The
/// browser has no use for the key, the url or the env here — so they never cross.
#[derive(serde::Serialize)]
pub struct GlobalView {
    pub id:            i64,
    pub name:          String,
    /// The catalog entry this instance came from. The UI needs it to tell which
    /// catalog rows are already enabled — the runtime name can be overridden, so
    /// matching on `name` alone would miss a renamed one.
    pub catalog_name:  Option<String>,
    pub friendly_name: Option<String>,
    pub description:   Option<String>,
    pub transport:     String,
    pub enabled:       bool,
    /// Whether the caller is actually granted this connector. An admin sees every
    /// global — including one they enabled for someone else and never granted
    /// themselves — so this is what separates "I can manage it" from "I can use it".
    pub can_use:       bool,
}

/// What the caller can reach or add on the Connectors page: the catalog entries they
/// may act on, plus the globally-active connectors.
///
/// The catalog list mixes both scopes on purpose — enabling a `global` entry is the
/// admin's counterpart to activating a `per_user` one (§7: one template, two runtimes),
/// so it is one list with a different verb per row rather than two sections.
pub async fn available(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Value>, ApiError> {
    let user = skald_core::db::users::get(skald.db(), &auth.user_id).await?
        .ok_or_else(|| ApiError::unauthorized("unknown user"))?;
    let manages_catalog =
        role_capabilities::has(skald.db(), &user.role_id, role_capabilities::MANAGE_CATALOG).await?;

    let granted_catalog: std::collections::HashSet<String> =
        mcp_catalog_access::catalog_names_for_user(skald.db(), &auth.user_id).await?
            .into_iter()
            .collect();
    let mut catalog: Vec<_> = mcp_catalog::list_for_scope(skald.db(), "per_user").await?
        .into_iter()
        // Deny-by-default: a user sees a per-user catalog entry only if the admin
        // granted it; a catalog manager sees every entry to curate it.
        .filter(|e| manages_catalog || granted_catalog.contains(&e.name))
        .collect();
    if manages_catalog {
        catalog.extend(mcp_catalog::list_for_scope(skald.db(), "global").await?);
    }

    let granted: std::collections::HashSet<String> =
        mcp_global_access::server_names_for_user(skald.db(), &auth.user_id).await?
            .into_iter()
            .collect();

    let globals: Vec<GlobalView> = mcp_global_servers::all(skald.db()).await?
        .into_iter()
        // A catalog manager needs to see globals they cannot themselves use, or an
        // entry enabled for someone else becomes invisible and unmanageable.
        .filter(|r| manages_catalog || granted.contains(&r.name))
        .map(|r| GlobalView {
            can_use:       granted.contains(&r.name),
            id:            r.id,
            name:          r.name,
            catalog_name:  r.catalog_name,
            friendly_name: r.friendly_name,
            description:   r.description,
            transport:     r.transport,
            enabled:       r.enabled,
        })
        .collect();

    Ok(Json(json!({ "catalog": catalog, "globals": globals })))
}

/// The connectors this user has already activated (per-user runtime).
pub async fn activated_list(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
) -> Result<Json<Vec<mcp_user_servers::McpUserServerRow>>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    Ok(Json(mcp_user_servers::all(&ctx.pool).await?))
}

#[derive(Deserialize)]
pub struct ActivateBody {
    /// A catalog entry to instantiate (per-user). Omit for a self-registered remote.
    pub catalog_name: Option<String>,
    /// Runtime name; defaults to the catalog name. Required for a self-registered remote.
    pub name:         Option<String>,
    /// Secrets/env the user supplies for this activation (stored encrypted in {userid}.db).
    pub env:          Option<HashMap<String, String>>,
    pub api_key:      Option<String>,
    // Self-registered remote only:
    pub url:          Option<String>,
    #[serde(default = "default_stdio")]
    pub transport:    String,
}

pub async fn activate(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<ActivateBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;

    // Resolve the row to insert from either the catalog or a self-registered remote.
    let insert = match &body.catalog_name {
        Some(cat_name) => {
            let entry = mcp_catalog::get_by_name(skald.db(), cat_name).await?
                .ok_or_else(|| ApiError::not_found(format!("no catalog entry `{cat_name}`")))?;
            if entry.scope != "per_user" {
                return Err(ApiError::bad_request("catalog entry is not a per-user connector"));
            }
            // Deny-by-default per-user access: the admin must have granted this user
            // the connector (`mcp_catalog_access`). This is the real boundary — the
            // `available` list only hides it in the UI.
            if !mcp_catalog_access::has_access(skald.db(), cat_name, &auth.user_id).await? {
                return Err(ApiError::forbidden(
                    "you are not authorized to use this connector — ask an admin to enable it for you",
                ));
            }
            let cap = if entry.source == "local_script" {
                role_capabilities::REGISTER_LOCAL_FROM_CATALOG
            } else {
                role_capabilities::REGISTER_REMOTE
            };
            require_cap(&skald, &auth.user_id, cap).await?;

            let name = body.name.clone().unwrap_or_else(|| entry.name.clone());
            reject_name_collision(&skald, &ctx.pool, &auth.user_id, &name).await?;

            // For a local script, install its folder into the container home and
            // point the command at the in-container path.
            let (command, args_json, script_rel_path) = if entry.source == "local_script" {
                let script = entry.script_path.clone()
                    .ok_or_else(|| ApiError::bad_request("catalog local_script entry has no script_path"))?;
                let container_path = install_connector_for_user(&auth.user_id, &name, &script)?;
                (entry.command.clone(), Some(json!([container_path]).to_string()), Some(container_path))
            } else {
                (entry.command.clone(), entry.args_json.clone(), None)
            };
            let env_json = body.env.as_ref()
                .and_then(|e| serde_json::to_string(e).ok())
                .or_else(|| entry.env_json.clone());

            // Reconcile node/python dependencies into the container before anything
            // tries to run the server (verify, the QR login, or a first message).
            // Blocking and one-time: the content-hash lock in `ensure_installed`
            // makes every later activation/login a no-op. A hard failure here is a
            // clear error rather than a connector that silently never starts.
            if entry.source == "local_script" {
                if let Some(script) = entry.script_path.as_deref() {
                    if let Ok((folder, _)) = skald_core::mcp::split_script_path(script) {
                        let container = skald_core::container::container_name(&auth.user_id);
                        skald_core::mcp::install::ensure_installed(&auth.user_id, &name, folder, &container)
                            .await
                            .map_err(|e| ApiError::bad_request(format!("dependency install failed: {e}")))?;
                    }
                }
            }

            // OAuth connectors do NOT activate directly (§15): the refresh token
            // comes from an interactive consent, not from the activation form. We
            // persist a PENDING row (files installed, command wired) and hand off to
            // `/mcp/oauth/start` → `/complete`, which obtains the token, flips the
            // row to `ready`, and starts the server. Nothing runs until then.
            if entry.auth_kind == "oauth" {
                let provider_name = entry.oauth_provider.as_deref().ok_or_else(|| {
                    ApiError::bad_request("this OAuth connector names no provider in the catalog")
                })?;
                // Fail early, clearly, if the admin has not configured the provider —
                // better than a dead pending row the user cannot complete.
                let provider = skald_core::db::oauth_providers::get(skald.db(), provider_name).await?
                    .ok_or_else(|| ApiError::bad_request(format!(
                        "the `{provider_name}` sign-in provider is not set up yet — \
                         an admin must add its client credentials first"
                    )))?;
                if provider.client_id.is_empty() {
                    return Err(ApiError::bad_request(format!(
                        "the `{provider_name}` sign-in provider has no client id configured"
                    )));
                }
                let id = mcp_user_servers::insert(&ctx.pool, mcp_user_servers::InsertUserServer {
                    name:                   &name,
                    catalog_name:           Some(&entry.name),
                    source:                 &entry.source,
                    transport:              &entry.transport,
                    command:                command.as_deref(),
                    args_json,
                    env_json,
                    url:                    entry.url.as_deref(),
                    api_key:                None,                 // obtained by the OAuth flow
                    oauth_provider:         Some(provider_name),
                    deliver_json:           entry.deliver_json.clone(),
                    script_rel_path:        script_rel_path.as_deref(),
                    verify_command:         None,
                    verify_script_rel_path: None,
                    auth_state:             "pending",
                }).await?;
                return Ok(Json(json!({
                    "id": id, "auth_state": "pending", "needs_oauth": true,
                })));
            }

            // QR (and other interactive-login) connectors, e.g. WhatsApp: unlike
            // OAuth there is no code to paste back — the server must RUN to produce
            // the QR, and the credential is the on-disk session it persists after the
            // scan. Insert a PENDING row, start the server so it emits a QR, and hand
            // off to the login panel, which polls `/mcp/login/status` until it reports
            // `ready` (flipping the row so `all_startable` picks it up next login).
            if entry.auth_kind == "qr" {
                let id = mcp_user_servers::insert(&ctx.pool, mcp_user_servers::InsertUserServer {
                    name:                   &name,
                    catalog_name:           Some(&entry.name),
                    source:                 &entry.source,
                    transport:              &entry.transport,
                    command:                command.as_deref(),
                    args_json,
                    env_json,
                    url:                    entry.url.as_deref(),
                    api_key:                None,       // the "credential" is the on-disk session
                    oauth_provider:         None,
                    deliver_json:           None,
                    script_rel_path:        script_rel_path.as_deref(),
                    verify_command:         None,
                    verify_script_rel_path: None,
                    auth_state:             "pending",
                }).await?;
                if let Some(row) = mcp_user_servers::get(&ctx.pool, id).await? {
                    let container = skald_core::container::container_name(&auth.user_id);
                    let spec = skald_core::mcp::user_row_spec_resolved(&row, &container, skald.db()).await;
                    // The QR only appears once the socket connects; ignore a start
                    // error here — the login panel surfaces the real state via polling.
                    let _ = ctx.user_mcp.start_server(spec).await;
                }
                return Ok(Json(json!({
                    "id": id, "auth_state": "pending", "needs_login": true, "login_kind": "qr",
                })));
            }

            mcp_user_servers::insert(&ctx.pool, mcp_user_servers::InsertUserServer {
                name:                   &name,
                catalog_name:           Some(&entry.name),
                source:                 &entry.source,
                transport:              &entry.transport,
                command:                command.as_deref(),
                args_json,
                env_json,
                url:                    entry.url.as_deref(),
                api_key:                body.api_key.as_deref(),
                oauth_provider:         None,
                deliver_json:           None,
                script_rel_path:        script_rel_path.as_deref(),
                verify_command:         entry.verify_command.as_deref(),
                verify_script_rel_path: None, // resolved when verify runs in-container
                auth_state:             "ready",
            }).await?
        }
        None => {
            // Self-registered remote (egress-only, §14) — needs `register_remote`.
            require_cap(&skald, &auth.user_id, role_capabilities::REGISTER_REMOTE).await?;
            let name = body.name.clone()
                .ok_or_else(|| ApiError::bad_request("a self-registered remote needs a `name`"))?;
            let url = body.url.clone()
                .ok_or_else(|| ApiError::bad_request("a self-registered remote needs a `url`"))?;
            reject_name_collision(&skald, &ctx.pool, &auth.user_id, &name).await?;
            mcp_user_servers::insert(&ctx.pool, mcp_user_servers::InsertUserServer {
                name:                   &name,
                catalog_name:           None,
                source:                 "remote",
                transport:              &body.transport,
                command:                None,
                args_json:              None,
                env_json:               body.env.as_ref().and_then(|e| serde_json::to_string(e).ok()),
                url:                    Some(&url),
                api_key:                body.api_key.as_deref(),
                oauth_provider:         None,
                deliver_json:           None,
                script_rel_path:        None,
                verify_command:         None,
                verify_script_rel_path: None,
                auth_state:             "ready",
            }).await?
        }
    };

    // Start it now in this user's runtime (container transport for stdio).
    let row = mcp_user_servers::get(&ctx.pool, insert).await?
        .ok_or_else(|| ApiError::bad_request("user server vanished after insert"))?;

    // Verify-before-start for catalog local_script connectors (a self-registered
    // remote has no verify step). The activation is persisted either way; a
    // failed verify flips auth_state to 'pending' so the user can retry without
    // retyping, and the row stays out of `all_startable` until a test passes.
    let verify = if row.source == "local_script" && row.verify_command.is_some() {
        match &row.catalog_name {
            Some(catalog_name) => match mcp_catalog::get_by_name(skald.db(), catalog_name).await? {
                Some(entry) => {
                    let report = run_verify_for_entry(
                        &skald, &auth, &entry, &row.name,
                        body.env.as_ref(), body.api_key.as_deref(),
                    ).await?;
                    if !report.skipped && !report.ok {
                        mcp_user_servers::set_auth_state(&ctx.pool, insert, "pending").await?;
                        return Ok(Json(json!({
                            "id": insert, "verify": report, "auth_state": "pending",
                        })));
                    }
                    report
                }
                None => skald_core::mcp::VerifyReport::skipped(),
            },
            None => skald_core::mcp::VerifyReport::skipped(),
        }
    } else {
        skald_core::mcp::VerifyReport::skipped()
    };

    let container = skald_core::container::container_name(&auth.user_id);
    let spec = skald_core::mcp::user_row_spec_resolved(&row, &container, skald.db()).await;
    match ctx.user_mcp.start_server(spec).await {
        Ok(tools) => Ok(Json(json!({
            "id": insert, "tools": tools, "verify": verify, "auth_state": "ready",
        }))),
        Err(e) => Ok(Json(json!({
            "id": insert, "error": e.to_string(), "verify": verify,
        }))),
    }
}

/// Rejects a per-user connector name that collides with an accessible global one
/// or an already-activated per-user one — so a bare grant string resolves to
/// exactly one runtime in `UserMcpView`.
async fn reject_name_collision(
    skald:   &Skald,
    pool:    &sqlx::SqlitePool,
    user_id: &str,
    name:    &str,
) -> Result<(), ApiError> {
    if mcp_user_servers::get_by_name(pool, name).await?.is_some() {
        return Err(ApiError::bad_request(format!("a connector named `{name}` is already activated")));
    }
    let globals = mcp_global_access::server_names_for_user(skald.db(), user_id).await?;
    if globals.iter().any(|g| g == name) {
        return Err(ApiError::bad_request(format!("`{name}` collides with a global connector you can access — choose another name")));
    }
    Ok(())
}

pub async fn deactivate(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    if let Some(row) = mcp_user_servers::get(&ctx.pool, id).await? {
        ctx.user_mcp.stop_server(&row.name);
    }
    mcp_user_servers::delete(&ctx.pool, id).await?;
    Ok(Json(json!({ "ok": true })))
}

// ── user: interactive OAuth login for a per-user connector (§15) ───────────────
//
// A pending consent lives only in RAM, keyed by an opaque `state`: it holds the
// PKCE verifier that a copy-pasteable authorization code is worthless without, plus
// which user + connector row it belongs to. The flow is stateless on disk — an
// abandoned consent is simply pruned, and a restart drops every in-flight flow (the
// user just starts again), mirroring the RAM-only session model.

struct PendingFlow {
    user_id:       String,
    server_id:     i64,
    verifier:      String,
    provider_name: String,
    created_at:    std::time::Instant,
}

/// How long a started-but-uncompleted consent stays valid.
const OAUTH_FLOW_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

static OAUTH_FLOWS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, PendingFlow>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Stores a pending flow, pruning any that timed out first. The lock is never held
/// across an `.await` — a single synchronous critical section.
fn oauth_flow_insert(state: String, flow: PendingFlow) {
    let mut map = OAUTH_FLOWS.lock().unwrap();
    map.retain(|_, f| f.created_at.elapsed() < OAUTH_FLOW_TTL);
    map.insert(state, flow);
}

/// Removes and returns a pending flow, or `None` if unknown or expired.
fn oauth_flow_take(state: &str) -> Option<PendingFlow> {
    let mut map = OAUTH_FLOWS.lock().unwrap();
    let flow = map.remove(state)?;
    (flow.created_at.elapsed() < OAUTH_FLOW_TTL).then_some(flow)
}

#[derive(Deserialize)]
pub struct OauthStartBody {
    /// The pending `mcp_user_servers` row (created by `activate`) to sign in.
    pub server_id: i64,
}

/// `POST /api/mcp/oauth/start` — begins the consent for a pending OAuth connector.
/// Returns the URL the user opens and an opaque `state` the caller echoes back to
/// `/complete`. Does not touch the provider or the network beyond building a URL.
pub async fn oauth_start(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<OauthStartBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let row = mcp_user_servers::get(&ctx.pool, body.server_id).await?
        .ok_or_else(|| ApiError::not_found("no such connector"))?;
    let provider_name = row.oauth_provider.as_deref()
        .ok_or_else(|| ApiError::bad_request("this connector does not use OAuth"))?;
    let provider = skald_core::db::oauth_providers::get(skald.db(), provider_name).await?
        .ok_or_else(|| ApiError::bad_request(format!("sign-in provider `{provider_name}` is not configured")))?;

    // The scopes to request live on the catalog entry (kept current), linked by the
    // row's snapshotted catalog name.
    let scopes = match &row.catalog_name {
        Some(cn) => mcp_catalog::get_by_name(skald.db(), cn).await?
            .map(|e| e.oauth_scopes())
            .unwrap_or_default(),
        None => Vec::new(),
    };

    let pkce = skald_core::mcp::oauth::generate_pkce();
    let state = skald_core::mcp::oauth::random_state();
    let auth_url = skald_core::mcp::oauth::build_consent_url(&provider, &scopes, &state, &pkce.challenge)
        .map_err(|e| ApiError::bad_request(e.to_string()))?;

    oauth_flow_insert(state.clone(), PendingFlow {
        user_id:       auth.user_id.clone(),
        server_id:     row.id,
        verifier:      pkce.verifier,
        provider_name: provider_name.to_string(),
        created_at:    std::time::Instant::now(),
    });

    Ok(Json(json!({ "auth_url": auth_url, "state": state })))
}

#[derive(Deserialize)]
pub struct OauthCompleteBody {
    /// The `state` returned by `/start`, identifying the pending flow.
    pub state: String,
    /// The authorization code the user pasted from the provider's page.
    pub code:  String,
}

/// `POST /api/mcp/oauth/complete` — exchanges the pasted code for a refresh token,
/// stores it, flips the connector to `ready`, and starts it in the user's runtime.
pub async fn oauth_complete(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<OauthCompleteBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let flow = oauth_flow_take(&body.state)
        .ok_or_else(|| ApiError::bad_request("this sign-in has expired — start it again"))?;
    // The flow is bound to the user who started it: a leaked `state` cannot let
    // someone finish another person's consent into their own connector.
    if flow.user_id != auth.user_id {
        return Err(ApiError::forbidden("this sign-in does not belong to you"));
    }
    let provider = skald_core::db::oauth_providers::get(skald.db(), &flow.provider_name).await?
        .ok_or_else(|| ApiError::bad_request("sign-in provider is no longer configured"))?;

    let token = skald_core::mcp::oauth::exchange_code(&provider, body.code.trim(), &flow.verifier)
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    let refresh = token.refresh_token.ok_or_else(|| ApiError::bad_request(
        "the provider returned no refresh token — revoke this app's access in your \
         account settings and try the sign-in again",
    ))?;
    mcp_user_servers::set_oauth_token(&ctx.pool, flow.server_id, &refresh).await?;

    // Start it now, with the credential resolved into the server's env (§15).
    let row = mcp_user_servers::get(&ctx.pool, flow.server_id).await?
        .ok_or_else(|| ApiError::bad_request("connector vanished after sign-in"))?;
    let container = skald_core::container::container_name(&auth.user_id);
    let spec = skald_core::mcp::user_row_spec_resolved(&row, &container, skald.db()).await;
    match ctx.user_mcp.start_server(spec).await {
        Ok(tools) => Ok(Json(json!({ "id": row.id, "tools": tools, "auth_state": "ready" }))),
        Err(e)    => Ok(Json(json!({ "id": row.id, "error": e.to_string(), "auth_state": "ready" }))),
    }
}

// ── user: interactive QR / device login for a per-user connector (§15) ─────────
//
// The generic seam for any connector whose login is neither an api-key nor an
// OAuth code-paste (WhatsApp's QR today; SSH / other device pairings later): the
// connector's server exposes a standard `login_status` tool returning
// `{state, qr?, message}`, and Skald calls it DIRECTLY (never the agent). Unlike
// OAuth, the server must be RUNNING to produce the credential (a QR the user
// scans), and the credential is the on-disk session it persists — so there is
// nothing to paste back, only a state to poll until it reports `ready`.

#[derive(Deserialize)]
pub struct LoginBody {
    /// The pending `mcp_user_servers` row to sign in.
    pub server_id: i64,
}

/// Starts `row`'s server in the user's runtime if it is not already live —
/// reconciling its deps first (a container recreated since activation may lack
/// them). Idempotent: a no-op when the server is already connected.
async fn ensure_user_server_running(
    skald:   &Skald,
    ctx:     &skald_core::skald::UserContext,
    user_id: &str,
    row:     &mcp_user_servers::McpUserServerRow,
) -> Result<(), ApiError> {
    if ctx.user_mcp.is_running(&row.name) {
        return Ok(());
    }
    let container = skald_core::container::container_name(user_id);
    skald_core::mcp::prepare_local_connector(skald.db(), user_id, &container, row).await;
    let spec = skald_core::mcp::user_row_spec_resolved(row, &container, skald.db()).await;
    ctx.user_mcp.start_server(spec).await
        .map_err(|e| ApiError::bad_request(format!("could not start the connector: {e}")))?;
    Ok(())
}

/// `POST /api/mcp/login/status` — polls a connector's interactive-login state.
/// Ensures the server is running, calls its `login_status` tool, and returns the
/// `{state, qr, message}` it reports (with `id`/`auth_state`). When the connector
/// reports `ready`, its row is flipped so `all_startable` starts it on the next
/// login. Safe to poll on an interval from the login panel.
pub async fn login_status(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<LoginBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let row = mcp_user_servers::get(&ctx.pool, body.server_id).await?
        .ok_or_else(|| ApiError::not_found("no such connector"))?;
    ensure_user_server_running(&skald, &ctx, &auth.user_id, &row).await?;

    let result = ctx.user_mcp.call(&row.name, "login_status", json!({})).await
        .map_err(|e| ApiError::bad_request(format!(
            "this connector has no interactive login (no login_status tool): {e}"
        )))?;
    // The tool returns a JSON string in a text part; fall back to a plain message
    // if a connector ever returns something else.
    let wire = result.to_wire();
    let mut v: Value = serde_json::from_str(&wire)
        .unwrap_or_else(|_| json!({ "state": "connecting", "message": wire }));
    let state = v.get("state").and_then(|s| s.as_str()).unwrap_or("connecting").to_string();

    if state == "ready" && row.auth_state != "ready" {
        mcp_user_servers::set_auth_state(&ctx.pool, row.id, "ready").await?;
    }
    if let Value::Object(ref mut m) = v {
        m.insert("id".into(), json!(row.id));
        m.insert("auth_state".into(), json!(if state == "ready" { "ready" } else { "pending" }));
    }
    Ok(Json(v))
}

/// `POST /api/mcp/login/reset` — re-arm the login (e.g. link a different phone).
/// Calls the connector's `logout` tool to clear the on-disk session and force a
/// fresh QR, and marks the row pending again.
pub async fn login_reset(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<LoginBody>,
) -> Result<Json<Value>, ApiError> {
    let ctx = require_context(&skald, &auth.user_id).await?;
    let row = mcp_user_servers::get(&ctx.pool, body.server_id).await?
        .ok_or_else(|| ApiError::not_found("no such connector"))?;
    ensure_user_server_running(&skald, &ctx, &auth.user_id, &row).await?;
    let _ = ctx.user_mcp.call(&row.name, "logout", json!({})).await;
    if row.auth_state == "ready" {
        mcp_user_servers::set_auth_state(&ctx.pool, row.id, "pending").await?;
    }
    Ok(Json(json!({ "ok": true, "id": row.id, "auth_state": "pending" })))
}
