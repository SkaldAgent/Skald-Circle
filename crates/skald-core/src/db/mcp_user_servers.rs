//! A user's activated per-user MCP connectors (blueprint §7/§14).
//!
//! Owner table in each `{userid}.db` — encrypted at rest (SQLCipher), so `api_key`
//! (a personal secret / OAuth refresh token) needs no column-level crypto.
//! `catalog_name` is a BARE `TEXT` snapshot of `mcp_catalog.name`, never a FK: an
//! owner→registry key would fail every INSERT under `PRAGMA foreign_keys=ON` in an
//! isolated file. Local-script connectors run INSIDE the user's container against a
//! script copied into the bind-mounted home (`script_rel_path`).

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct McpUserServerRow {
    pub id:                     i64,
    pub name:                   String,
    /// Bare snapshot of the originating `mcp_catalog.name`; NULL for a
    /// self-registered remote.
    pub catalog_name:           Option<String>,
    /// 'remote' | 'local_script'.
    pub source:                 String,
    pub transport:              String,
    pub command:                Option<String>,
    pub args_json:              Option<String>,
    pub env_json:               Option<String>,
    pub url:                    Option<String>,
    /// Per-user secret. For an OAuth connector this holds the refresh token; empty
    /// until the OAuth flow completes (`auth_state='pending'`).
    pub api_key:                Option<String>,
    /// oauth: snapshot of the catalog's `oauth_provider` (which app issued the token).
    pub oauth_provider:         Option<String>,
    /// oauth: snapshot of the catalog's delivery spec `{as,format,env,path}`.
    pub deliver_json:           Option<String>,
    /// Container path of the copied script, for a `local_script`.
    pub script_rel_path:        Option<String>,
    /// Snapshot of `mcp_catalog.verify_command` (NULL = no test).
    pub verify_command:         Option<String>,
    /// Container path of the verify script, if any.
    pub verify_script_rel_path: Option<String>,
    /// 'pending' | 'ready' — the verify-before-save gate.
    pub auth_state:             String,
    pub enabled:                bool,
}

impl McpUserServerRow {
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

    /// The credential delivery spec (`{as,format,env,path}`), if this is an OAuth
    /// connector that snapshotted one.
    pub fn deliver(&self) -> Option<crate::mcp::DeliverSpec> {
        self.deliver_json.as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
    }
}

const SELECT: &str =
    "SELECT id, name, catalog_name, source, transport, command, args_json, env_json, url, \
            api_key, oauth_provider, deliver_json, script_rel_path, verify_command, \
            verify_script_rel_path, auth_state, enabled \
     FROM mcp_user_servers";

// ── Reads ────────────────────────────────────────────────────────────────────

pub async fn all(pool: &SqlitePool) -> Result<Vec<McpUserServerRow>> {
    let rows = sqlx::query_as::<_, McpUserServerRow>(sqlx::AssertSqlSafe(format!("{SELECT} ORDER BY name")))
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// The enabled, `auth_state='ready'` connectors — the set the per-user runtime
/// starts at login. A 'pending' one is activated but not yet authenticated.
pub async fn all_startable(pool: &SqlitePool) -> Result<Vec<McpUserServerRow>> {
    let rows = sqlx::query_as::<_, McpUserServerRow>(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE enabled = 1 AND auth_state = 'ready' ORDER BY name"
    )))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<McpUserServerRow>> {
    let row = sqlx::query_as::<_, McpUserServerRow>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE id = ?")))
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn get_by_name(pool: &SqlitePool, name: &str) -> Result<Option<McpUserServerRow>> {
    let row = sqlx::query_as::<_, McpUserServerRow>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE name = ?")))
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

// ── Writes ───────────────────────────────────────────────────────────────────

pub struct InsertUserServer<'a> {
    pub name:                   &'a str,
    pub catalog_name:           Option<&'a str>,
    pub source:                 &'a str,
    pub transport:              &'a str,
    pub command:                Option<&'a str>,
    pub args_json:              Option<String>,
    pub env_json:               Option<String>,
    pub url:                    Option<&'a str>,
    pub api_key:                Option<&'a str>,
    pub oauth_provider:         Option<&'a str>,
    pub deliver_json:           Option<String>,
    pub script_rel_path:        Option<&'a str>,
    pub verify_command:         Option<&'a str>,
    pub verify_script_rel_path: Option<&'a str>,
    pub auth_state:             &'a str,
}

pub async fn insert(pool: &SqlitePool, s: InsertUserServer<'_>) -> Result<i64> {
    let id = sqlx::query(
        "INSERT INTO mcp_user_servers
            (name, catalog_name, source, transport, command, args_json, env_json, url, api_key,
             oauth_provider, deliver_json, script_rel_path, verify_command, verify_script_rel_path,
             auth_state, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, 1)",
    )
    .bind(s.name)
    .bind(s.catalog_name)
    .bind(s.source)
    .bind(s.transport)
    .bind(s.command)
    .bind(s.args_json)
    .bind(s.env_json)
    .bind(s.url)
    .bind(s.api_key)
    .bind(s.oauth_provider)
    .bind(s.deliver_json)
    .bind(s.script_rel_path)
    .bind(s.verify_command)
    .bind(s.verify_script_rel_path)
    .bind(s.auth_state)
    .execute(pool)
    .await?
    .last_insert_rowid();
    Ok(id)
}

/// Stores a freshly-obtained OAuth refresh token and flips the connector to
/// `ready`, in one write — the completion of the §15 login flow.
pub async fn set_oauth_token(pool: &SqlitePool, id: i64, refresh_token: &str) -> Result<()> {
    sqlx::query("UPDATE mcp_user_servers SET api_key = ?1, auth_state = 'ready' WHERE id = ?2")
        .bind(refresh_token)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_enabled(pool: &SqlitePool, id: i64, enabled: bool) -> Result<()> {
    sqlx::query("UPDATE mcp_user_servers SET enabled = ?1 WHERE id = ?2")
        .bind(enabled as i64)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_auth_state(pool: &SqlitePool, id: i64, auth_state: &str) -> Result<()> {
    sqlx::query("UPDATE mcp_user_servers SET auth_state = ?1 WHERE id = ?2")
        .bind(auth_state)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM mcp_user_servers WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
