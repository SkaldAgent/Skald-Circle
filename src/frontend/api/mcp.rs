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

use skald_core::db::{mcp_catalog, mcp_global_access, mcp_global_servers, mcp_user_servers, role_capabilities};
use skald_core::skald::Skald;

use super::guard::AuthUser;
use super::{require_context, ApiError};

// ── helpers ───────────────────────────────────────────────────────────────────

/// Fails with 403 unless the caller's role holds `cap` (admin holds everything).
async fn require_cap(skald: &Skald, user_id: &str, cap: &str) -> Result<(), ApiError> {
    let user = skald_core::db::users::get(skald.db(), user_id).await?
        .ok_or_else(|| ApiError::unauthorized("unknown user"))?;
    if role_capabilities::has(skald.db(), &user.role_id, cap).await? {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!("your role lacks the capability `{cap}`")))
    }
}

fn to_json_opt<T: serde::Serialize>(v: &Option<T>) -> Option<String> {
    v.as_ref().and_then(|x| serde_json::to_string(x).ok())
}

/// Copies a vetted catalog script from `./scripts/<script_path>` into the user's
/// bind-mounted home under `.skald/mcp/<name>/`, and returns the path it will have
/// INSIDE the container (`/root/.skald/mcp/...`). The home is the only durable
/// zone (§6), so the script survives a container recreate. Single files only for
/// now — directory-tree scripts (e.g. whatsapp_mcp/) are a follow-up.
fn copy_script_into_home(user_id: &str, name: &str, script_path: &str) -> Result<String, ApiError> {
    let wd = std::env::current_dir()
        .map_err(|e| ApiError::bad_request(format!("cannot resolve working directory: {e}")))?;
    let src = wd.join("scripts").join(script_path);
    if !src.is_file() {
        return Err(ApiError::bad_request(format!(
            "catalog script `scripts/{script_path}` not found or not a file \
             (directory-tree scripts are not supported yet)"
        )));
    }
    let basename = src.file_name()
        .ok_or_else(|| ApiError::bad_request("invalid script_path"))?
        .to_string_lossy().to_string();
    let dest_dir = wd.join(skald_core::container::HOMES_DIR)
        .join(user_id).join(".skald").join("mcp").join(name);
    std::fs::create_dir_all(&dest_dir)
        .map_err(|e| ApiError::bad_request(format!("failed to create script dir: {e}")))?;
    std::fs::copy(&src, dest_dir.join(&basename))
        .map_err(|e| ApiError::bad_request(format!("failed to copy script: {e}")))?;
    Ok(format!("/root/.skald/mcp/{name}/{basename}"))
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
        role_filter:        to_json_opt(&body.role_filter),
        friendly_name:      body.friendly_name.as_deref(),
        description:        body.description.as_deref(),
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
    // Snapshot the concrete config from the catalog; the admin supplies the secret.
    let id = mcp_global_servers::upsert(skald.db(), mcp_global_servers::UpsertGlobal {
        name:          &name,
        catalog_name:  Some(&entry.name),
        transport:     &entry.transport,
        command:       entry.command.as_deref(),
        args_json:     entry.args_json.clone(),
        env_json:      body.env.as_ref().and_then(|e| serde_json::to_string(e).ok()).or_else(|| entry.env_json.clone()),
        url:           entry.url.as_deref(),
        api_key:       body.api_key.as_deref(),
        friendly_name: entry.friendly_name.as_deref(),
        description:   entry.description.as_deref(),
    }).await?;

    // Start it now in the global runtime (host transport).
    let row = mcp_global_servers::get(skald.db(), id).await?
        .ok_or_else(|| ApiError::bad_request("global server vanished after upsert"))?;
    let spec = skald_core::mcp::global_row_spec(&row);
    match skald.mcp().start_server(spec).await {
        Ok(tools) => Ok(Json(json!({ "id": id, "tools": tools }))),
        Err(e)    => Ok(Json(json!({ "id": id, "error": e.to_string() }))),
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

    let mut catalog: Vec<_> = mcp_catalog::list_for_scope(skald.db(), "per_user").await?
        .into_iter()
        .filter(|e| e.allowed_for_role(&user.role_id))
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
    let user = skald_core::db::users::get(skald.db(), &auth.user_id).await?
        .ok_or_else(|| ApiError::unauthorized("unknown user"))?;

    // Resolve the row to insert from either the catalog or a self-registered remote.
    let insert = match &body.catalog_name {
        Some(cat_name) => {
            let entry = mcp_catalog::get_by_name(skald.db(), cat_name).await?
                .ok_or_else(|| ApiError::not_found(format!("no catalog entry `{cat_name}`")))?;
            if entry.scope != "per_user" {
                return Err(ApiError::bad_request("catalog entry is not a per-user connector"));
            }
            if !entry.allowed_for_role(&user.role_id) {
                return Err(ApiError::forbidden("your role may not activate this connector"));
            }
            let cap = if entry.source == "local_script" {
                role_capabilities::REGISTER_LOCAL_FROM_CATALOG
            } else {
                role_capabilities::REGISTER_REMOTE
            };
            require_cap(&skald, &auth.user_id, cap).await?;

            let name = body.name.clone().unwrap_or_else(|| entry.name.clone());
            reject_name_collision(&skald, &ctx.pool, &auth.user_id, &name).await?;

            // For a local script, copy it into the container home and point the
            // command at the in-container path.
            let (command, args_json, script_rel_path) = if entry.source == "local_script" {
                let script = entry.script_path.clone()
                    .ok_or_else(|| ApiError::bad_request("catalog local_script entry has no script_path"))?;
                let container_path = copy_script_into_home(&auth.user_id, &name, &script)?;
                (entry.command.clone(), Some(json!([container_path]).to_string()), Some(container_path))
            } else {
                (entry.command.clone(), entry.args_json.clone(), None)
            };

            mcp_user_servers::insert(&ctx.pool, mcp_user_servers::InsertUserServer {
                name:            &name,
                catalog_name:    Some(&entry.name),
                source:          &entry.source,
                transport:       &entry.transport,
                command:         command.as_deref(),
                args_json,
                env_json:        body.env.as_ref().and_then(|e| serde_json::to_string(e).ok()).or_else(|| entry.env_json.clone()),
                url:             entry.url.as_deref(),
                api_key:         body.api_key.as_deref(),
                script_rel_path: script_rel_path.as_deref(),
                auth_state:      "ready",
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
                name:            &name,
                catalog_name:    None,
                source:          "remote",
                transport:       &body.transport,
                command:         None,
                args_json:       None,
                env_json:        body.env.as_ref().and_then(|e| serde_json::to_string(e).ok()),
                url:             Some(&url),
                api_key:         body.api_key.as_deref(),
                script_rel_path: None,
                auth_state:      "ready",
            }).await?
        }
    };

    // Start it now in this user's runtime (container transport for stdio).
    let row = mcp_user_servers::get(&ctx.pool, insert).await?
        .ok_or_else(|| ApiError::bad_request("user server vanished after insert"))?;
    let container = skald_core::container::container_name(&auth.user_id);
    let spec = skald_core::mcp::user_row_spec(&row, &container);
    match ctx.user_mcp.start_server(spec).await {
        Ok(tools) => Ok(Json(json!({ "id": insert, "tools": tools }))),
        Err(e)    => Ok(Json(json!({ "id": insert, "error": e.to_string() }))),
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
