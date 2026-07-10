use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerRow {
    pub id:            i64,
    pub name:          String,
    pub transport:     String,
    pub command:       Option<String>,
    pub args_json:     Option<String>,
    pub env_json:      Option<String>,
    pub url:           Option<String>,
    pub api_key:       Option<String>,
    pub description:   Option<String>,
    pub friendly_name: Option<String>,
    pub enabled:       bool,
}

impl McpServerRow {
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

type RawRow = (i64, String, String, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, Option<String>, i64);

fn from_raw(r: RawRow) -> McpServerRow {
    McpServerRow {
        id:            r.0,
        name:          r.1,
        transport:     r.2,
        command:       r.3,
        args_json:     r.4,
        env_json:      r.5,
        url:           r.6,
        api_key:       r.7,
        description:   r.8,
        friendly_name: r.9,
        enabled:       r.10 != 0,
    }
}

const SELECT: &str =
    "SELECT id, name, transport, command, args_json, env_json, url, api_key, description, friendly_name, enabled \
     FROM mcp_servers";

pub async fn all(pool: &SqlitePool) -> Result<Vec<McpServerRow>> {
    let rows = sqlx::query_as::<_, RawRow>(sqlx::AssertSqlSafe(format!("{SELECT} ORDER BY name")))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(from_raw).collect())
}

pub async fn all_enabled(pool: &SqlitePool) -> Result<Vec<McpServerRow>> {
    let rows = sqlx::query_as::<_, RawRow>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE enabled = 1 ORDER BY name")))
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(from_raw).collect())
}

pub struct UpsertParams<'a> {
    pub name:          &'a str,
    pub transport:     &'a str,
    pub command:       Option<&'a str>,
    pub args_json:     Option<String>,
    pub env_json:      Option<String>,
    pub url:           Option<&'a str>,
    pub api_key:       Option<&'a str>,
    pub description:   Option<&'a str>,
    pub friendly_name: Option<&'a str>,
}

pub async fn upsert(pool: &SqlitePool, p: UpsertParams<'_>) -> Result<i64> {
    let row = sqlx::query_as::<_, (i64,)>(
        "INSERT INTO mcp_servers (name, transport, command, args_json, env_json, url, api_key, description, friendly_name, enabled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1)
         ON CONFLICT(name) DO UPDATE SET
             transport     = excluded.transport,
             command       = excluded.command,
             args_json     = excluded.args_json,
             env_json      = excluded.env_json,
             url           = excluded.url,
             api_key       = excluded.api_key,
             description   = excluded.description,
             friendly_name = excluded.friendly_name,
             enabled       = 1
         RETURNING id",
    )
    .bind(p.name)
    .bind(p.transport)
    .bind(p.command)
    .bind(p.args_json)
    .bind(p.env_json)
    .bind(p.url)
    .bind(p.api_key)
    .bind(p.description)
    .bind(p.friendly_name)
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

pub async fn set_enabled(pool: &SqlitePool, name: &str, enabled: bool) -> Result<()> {
    sqlx::query("UPDATE mcp_servers SET enabled = ?1 WHERE name = ?2")
        .bind(enabled as i64)
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, name: &str) -> Result<()> {
    sqlx::query("DELETE FROM mcp_servers WHERE name = ?1")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}
