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
    /// local_script: the vetted source path under `./scripts`.
    pub script_path:        Option<String>,
    /// Names of the env/secret keys the activation UI must collect (never values).
    pub config_schema_json: Option<String>,
    /// 'none'|'api_key'|'oauth'|'qr'|'ssh_key'. Only 'none'/'api_key' are wired now.
    pub auth_kind:          String,
    /// JSON array of role ids allowed to activate this; NULL = all roles (§15).
    pub role_filter:        Option<String>,
    pub friendly_name:      Option<String>,
    pub description:        Option<String>,
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
}

const SELECT: &str =
    "SELECT id, name, scope, source, transport, command, args_json, env_json, url, \
            script_path, config_schema_json, auth_kind, role_filter, friendly_name, \
            description, created_at \
     FROM mcp_catalog";

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
    pub role_filter:        Option<String>,
    pub friendly_name:      Option<&'a str>,
    pub description:        Option<&'a str>,
}

pub async fn upsert(pool: &SqlitePool, e: UpsertCatalog<'_>) -> Result<i64> {
    let row = sqlx::query_as::<_, (i64,)>(
        "INSERT INTO mcp_catalog
            (name, scope, source, transport, command, args_json, env_json, url,
             script_path, config_schema_json, auth_kind, role_filter, friendly_name, description)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
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
             role_filter        = excluded.role_filter,
             friendly_name      = excluded.friendly_name,
             description        = excluded.description
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
    .bind(e.role_filter)
    .bind(e.friendly_name)
    .bind(e.description)
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
