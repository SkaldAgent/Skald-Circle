//! Globally-active MCP connectors (blueprint §7/§15b): shared, stateless servers
//! (web-search, Tavily…) that run on the HOST and are offered to every user the
//! admin grants access to (`mcp_global_access`).
//!
//! Registry table in `system.db`. The global secret (the admin's API key) is fine
//! here — `system.db` is admin-owned (§4). `catalog_name` is a registry→registry
//! FK to `mcp_catalog(name)` (both in this file), which is allowed.

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct McpGlobalServerRow {
    pub id:                 i64,
    pub name:               String,
    pub catalog_name:       Option<String>,
    pub transport:          String,
    pub command:            Option<String>,
    pub args_json:          Option<String>,
    pub env_json:           Option<String>,
    pub url:                Option<String>,
    pub api_key:            Option<String>,
    /// Snapshot of `mcp_catalog.verify_command` (NULL = no test).
    pub verify_command:     Option<String>,
    /// Absolute host path of the verify script, if any.
    pub verify_script_path: Option<String>,
    pub friendly_name:      Option<String>,
    pub description:        Option<String>,
    pub enabled:            bool,
}

impl McpGlobalServerRow {
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
}

const SELECT: &str =
    "SELECT id, name, catalog_name, transport, command, args_json, env_json, url, \
            api_key, verify_command, verify_script_path, friendly_name, description, enabled \
     FROM mcp_global_servers";

// ── Reads ────────────────────────────────────────────────────────────────────

pub async fn all(pool: &SqlitePool) -> Result<Vec<McpGlobalServerRow>> {
    let rows = sqlx::query_as::<_, McpGlobalServerRow>(sqlx::AssertSqlSafe(format!("{SELECT} ORDER BY name")))
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn all_enabled(pool: &SqlitePool) -> Result<Vec<McpGlobalServerRow>> {
    let rows = sqlx::query_as::<_, McpGlobalServerRow>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE enabled = 1 ORDER BY name")))
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<McpGlobalServerRow>> {
    let row = sqlx::query_as::<_, McpGlobalServerRow>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE id = ?")))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn get_by_name(pool: &SqlitePool, name: &str) -> Result<Option<McpGlobalServerRow>> {
    let row = sqlx::query_as::<_, McpGlobalServerRow>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE name = ?")))
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

// ── Writes ───────────────────────────────────────────────────────────────────

pub struct UpsertGlobal<'a> {
    pub name:               &'a str,
    pub catalog_name:       Option<&'a str>,
    pub transport:          &'a str,
    pub command:            Option<&'a str>,
    pub args_json:          Option<String>,
    pub env_json:           Option<String>,
    pub url:                Option<&'a str>,
    pub api_key:            Option<&'a str>,
    pub verify_command:     Option<&'a str>,
    pub verify_script_path: Option<&'a str>,
    pub friendly_name:      Option<&'a str>,
    pub description:        Option<&'a str>,
}

pub async fn upsert(pool: &SqlitePool, p: UpsertGlobal<'_>) -> Result<i64> {
    let row = sqlx::query_as::<_, (i64,)>(
        "INSERT INTO mcp_global_servers
            (name, catalog_name, transport, command, args_json, env_json, url, api_key,
             verify_command, verify_script_path, friendly_name, description, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1)
         ON CONFLICT(name) DO UPDATE SET
             catalog_name       = excluded.catalog_name,
             transport          = excluded.transport,
             command            = excluded.command,
             args_json          = excluded.args_json,
             env_json           = excluded.env_json,
             url                = excluded.url,
             api_key            = excluded.api_key,
             verify_command     = excluded.verify_command,
             verify_script_path = excluded.verify_script_path,
             friendly_name      = excluded.friendly_name,
             description        = excluded.description,
             enabled            = 1
         RETURNING id",
    )
    .bind(p.name)
    .bind(p.catalog_name)
    .bind(p.transport)
    .bind(p.command)
    .bind(p.args_json)
    .bind(p.env_json)
    .bind(p.url)
    .bind(p.api_key)
    .bind(p.verify_command)
    .bind(p.verify_script_path)
    .bind(p.friendly_name)
    .bind(p.description)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

pub async fn set_enabled(pool: &SqlitePool, id: i64, enabled: bool) -> Result<()> {
    sqlx::query("UPDATE mcp_global_servers SET enabled = ?1 WHERE id = ?2")
        .bind(enabled as i64)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM mcp_global_servers WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
