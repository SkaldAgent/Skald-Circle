//! The admin-curated catalog of installable MCP connectors (blueprint §14/§15).
//!
//! Registry table in `system.db`: instance-wide, listable without any user key so
//! the "Connectors" UI can render it. Each entry is a *template* — a per-user
//! connector is later instantiated from it into a `{userid}.db`
//! (`mcp_user_servers`), or a global one is enabled by the admin
//! (`mcp_global_servers`). No live credential ever lands here: `config_schema_json`
//! only names the env/secret keys an activation must collect.

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct McpCatalogRow {
    pub id:                 i64,
    pub name:               String,
    /// 'per_user' | 'global' — which category (§15) this entry can be activated as.
    pub scope:              String,
    /// 'remote' | 'local_script' — the §14 risk axis.
    pub source:             String,
    pub transport:          String,
    pub command:            Option<String>,
    pub args_json:          Option<String>,
    pub env_json:           Option<String>,
    pub url:                Option<String>,
    /// local_script: the vetted entry file, as `<connector>/<file>` under
    /// `./connectors` (see [`crate::mcp::install`]).
    pub script_path:        Option<String>,
    /// JSON array of `{name,label,description,required,secret,example,default}` objects
    /// describing the env/secret fields the activation UI must collect (never values).
    pub config_schema_json: Option<String>,
    /// 'none'|'api_key'|'oauth'|'qr'|'ssh_key'.
    pub auth_kind:          String,
    /// oauth: slug into `oauth_providers.name` (which app to consent to).
    pub oauth_provider:     Option<String>,
    /// oauth: JSON array of the scopes this connector requests at consent.
    pub oauth_scopes_json:  Option<String>,
    /// oauth: JSON `{as,format,env,path}` — how Skald delivers the obtained
    /// credential to the connector's server process (§15).
    pub deliver_json:       Option<String>,
    /// JSON array of role ids allowed to activate this; NULL = all roles (§15).
    pub role_filter:        Option<String>,
    /// Shell command run before persisting an activation (verify-before-save).
    pub verify_command:     Option<String>,
    /// Script file the verify command references (e.g. `verify.py`), if any.
    pub verify_script_path: Option<String>,
    /// Icon file *inside* `./connectors/<name>/`, if the feed shipped one. Stored
    /// rather than derived because the manifest names its icons freely (`.png` for
    /// one connector, `.svg` for the next), and the browser cannot guess.
    pub icon_small_path:    Option<String>,
    pub icon_large_path:    Option<String>,
    pub friendly_name:      Option<String>,
    pub description:        Option<String>,
    /// Manifest-declared friendly tool names: a JSON array of `{name, display_name}`
    /// snapshotting the connector's `tools[]` block. Drives the UI card title for an
    /// `mcp__<server>__<tool>` call (override > live MCP `title` > prettified name).
    /// NULL when the manifest declares none.
    pub tool_meta_json:     Option<String>,
    /// Marketplace build number — the **comparison key** for updates (a feed entry
    /// with a higher `version` than this installed one is "update available").
    /// Monotonic per connector; `version_string`/`version_release_date` are display
    /// only. NULL for a pre-versioning or manually-added entry.
    pub version:                Option<i64>,
    pub version_string:         Option<String>,
    pub version_release_date:   Option<String>,
    pub created_at:         String,
}

impl McpCatalogRow {
    pub fn args(&self) -> Vec<String> {
        self.args_json.as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    pub fn env(&self) -> HashMap<String, String> {
        self.env_json.as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }

    /// The role ids allowed to activate this entry, or `None` when unrestricted.
    pub fn allowed_roles(&self) -> Option<Vec<String>> {
        self.role_filter.as_deref()
            .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
    }

    /// Whether a user in `role_id` may activate this entry (§15 per-role catalog).
    pub fn allowed_for_role(&self, role_id: &str) -> bool {
        match self.allowed_roles() {
            None => true,
            Some(roles) => roles.iter().any(|r| r == role_id),
        }
    }

    /// The OAuth scopes this connector requests at consent, or empty when it is not
    /// an OAuth connector.
    pub fn oauth_scopes(&self) -> Vec<String> {
        self.oauth_scopes_json.as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }
}

const SELECT: &str =
    "SELECT id, name, scope, source, transport, command, args_json, env_json, url, \
            script_path, config_schema_json, auth_kind, oauth_provider, oauth_scopes_json, \
            deliver_json, role_filter, verify_command, \
            verify_script_path, icon_small_path, icon_large_path, friendly_name, \
            description, tool_meta_json, version, version_string, version_release_date, created_at \
     FROM mcp_catalog";

/// Parses a catalog row's `tool_meta_json` (a `[{name, display_name}]` array) into
/// the `tool name → display title` override map the runtime feeds into
/// [`McpServerSpec::tool_titles`]. Empty on NULL or malformed JSON — the runtime then
/// falls back to the server's live MCP `title` and finally a prettified raw name.
pub fn parse_tool_titles(tool_meta_json: Option<&str>) -> HashMap<String, String> {
    #[derive(serde::Deserialize)]
    struct ToolMeta { name: String, display_name: Option<String> }
    tool_meta_json
        .and_then(|s| serde_json::from_str::<Vec<ToolMeta>>(s).ok())
        .map(|metas| {
            metas.into_iter()
                .filter_map(|m| m.display_name.map(|dn| (m.name, dn)))
                .collect()
        })
        .unwrap_or_default()
}

// ── Reads ────────────────────────────────────────────────────────────────────

pub async fn list(pool: &SqlitePool) -> Result<Vec<McpCatalogRow>> {
    let rows = sqlx::query_as::<_, McpCatalogRow>(sqlx::AssertSqlSafe(format!("{SELECT} ORDER BY name")))
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Catalog entries in a given scope ('per_user' | 'global').
pub async fn list_for_scope(pool: &SqlitePool, scope: &str) -> Result<Vec<McpCatalogRow>> {
    let rows = sqlx::query_as::<_, McpCatalogRow>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE scope = ? ORDER BY name")))
        .bind(scope)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn get_by_name(pool: &SqlitePool, name: &str) -> Result<Option<McpCatalogRow>> {
    let row = sqlx::query_as::<_, McpCatalogRow>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE name = ?")))
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<McpCatalogRow>> {
    let row = sqlx::query_as::<_, McpCatalogRow>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE id = ?")))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

// ── Writes ───────────────────────────────────────────────────────────────────

/// Fields for creating or updating a catalog entry (keyed on `name`).
pub struct UpsertCatalog<'a> {
    pub name:               &'a str,
    pub scope:              &'a str,
    pub source:             &'a str,
    pub transport:          &'a str,
    pub command:            Option<&'a str>,
    pub args_json:          Option<String>,
    pub env_json:           Option<String>,
    pub url:                Option<&'a str>,
    pub script_path:        Option<&'a str>,
    pub config_schema_json: Option<String>,
    pub auth_kind:          &'a str,
    pub oauth_provider:     Option<&'a str>,
    pub oauth_scopes_json:  Option<String>,
    pub deliver_json:       Option<String>,
    pub role_filter:        Option<String>,
    pub verify_command:     Option<&'a str>,
    pub verify_script_path: Option<&'a str>,
    pub icon_small_path:    Option<&'a str>,
    pub icon_large_path:    Option<&'a str>,
    pub friendly_name:      Option<&'a str>,
    pub description:        Option<&'a str>,
    /// JSON array of `{name, display_name}` snapshotting the manifest's `tools[]`.
    pub tool_meta_json:     Option<String>,
    /// Versioning (from the feed). All three `None` for the admin's manual form,
    /// which COALESCEs them away rather than blanking an installed entry's version.
    pub version:                Option<i64>,
    pub version_string:         Option<&'a str>,
    pub version_release_date:   Option<&'a str>,
}

pub async fn upsert(pool: &SqlitePool, e: UpsertCatalog<'_>) -> Result<i64> {
    let row = sqlx::query_as::<_, (i64,)>(
        "INSERT INTO mcp_catalog
            (name, scope, source, transport, command, args_json, env_json, url,
             script_path, config_schema_json, auth_kind, oauth_provider, oauth_scopes_json,
             deliver_json, role_filter, verify_command,
             verify_script_path, icon_small_path, icon_large_path, friendly_name, description,
             tool_meta_json, version, version_string, version_release_date)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)
         ON CONFLICT(name) DO UPDATE SET
             scope              = excluded.scope,
             source             = excluded.source,
             transport          = excluded.transport,
             command            = excluded.command,
             args_json          = excluded.args_json,
             env_json           = excluded.env_json,
             url                = excluded.url,
             script_path        = excluded.script_path,
             config_schema_json = excluded.config_schema_json,
             auth_kind          = excluded.auth_kind,
             oauth_provider     = excluded.oauth_provider,
             oauth_scopes_json  = excluded.oauth_scopes_json,
             deliver_json       = excluded.deliver_json,
             role_filter        = excluded.role_filter,
             verify_command     = excluded.verify_command,
             verify_script_path = excluded.verify_script_path,
             -- Icons belong to whoever installed the files, not to whoever last
             -- edited the row: COALESCE keeps them when an admin saves the catalog
             -- form (which knows nothing about icons and would otherwise blank
             -- them), while a reinstall still updates them.
             icon_small_path    = COALESCE(excluded.icon_small_path, mcp_catalog.icon_small_path),
             icon_large_path    = COALESCE(excluded.icon_large_path, mcp_catalog.icon_large_path),
             friendly_name      = excluded.friendly_name,
             description        = excluded.description,
             -- Manifest tool titles are installer-owned like icons: COALESCE so the
             -- admin's manual catalog form (which never sends them) can't blank them.
             tool_meta_json     = COALESCE(excluded.tool_meta_json, mcp_catalog.tool_meta_json),
             -- Version fields come from the feed on (re)install; the admin's manual
             -- form passes NULL, so COALESCE keeps the installed version rather than
             -- wiping it (same rationale as icons above).
             version              = COALESCE(excluded.version, mcp_catalog.version),
             version_string       = COALESCE(excluded.version_string, mcp_catalog.version_string),
             version_release_date = COALESCE(excluded.version_release_date, mcp_catalog.version_release_date)
         RETURNING id",
    )
    .bind(e.name)
    .bind(e.scope)
    .bind(e.source)
    .bind(e.transport)
    .bind(e.command)
    .bind(e.args_json)
    .bind(e.env_json)
    .bind(e.url)
    .bind(e.script_path)
    .bind(e.config_schema_json)
    .bind(e.auth_kind)
    .bind(e.oauth_provider)
    .bind(e.oauth_scopes_json)
    .bind(e.deliver_json)
    .bind(e.role_filter)
    .bind(e.verify_command)
    .bind(e.verify_script_path)
    .bind(e.icon_small_path)
    .bind(e.icon_large_path)
    .bind(e.friendly_name)
    .bind(e.description)
    .bind(e.tool_meta_json)
    .bind(e.version)
    .bind(e.version_string)
    .bind(e.version_release_date)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

pub async fn delete(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM mcp_catalog WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
