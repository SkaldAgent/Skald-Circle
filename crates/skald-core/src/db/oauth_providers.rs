//! OAuth identity providers for per-user connectors (blueprint §15).
//!
//! Registry table in `system.db`: one row per provider (Google, …), keyed by a
//! stable slug that `mcp_catalog.oauth_provider` references. A single provider row
//! covers every service that provider exposes (Gmail, Calendar, Drive) — the
//! client credentials live here, the per-connector scopes in the catalog.
//!
//! `client_secret` is a household/global secret the admin owns (§4/§15b), so it is
//! fine in the admin-readable file. Per-user refresh tokens never land here.

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct OauthProviderRow {
    pub name:          String,
    pub display_name:  String,
    pub auth_url:      String,
    pub token_url:     String,
    pub client_id:     String,
    /// Never leaves the process for the browser — see [`OauthProviderView`].
    pub client_secret: String,
    pub redirect_uri:  String,
    pub extra_params:  Option<String>,
    pub created_at:    String,
    pub updated_at:    String,
}

impl OauthProviderRow {
    /// Extra authorization-endpoint params (e.g. `access_type=offline`,
    /// `prompt=consent`) merged into the consent URL. Google needs both to return a
    /// refresh token; a provider that needs neither leaves this NULL.
    pub fn extra(&self) -> HashMap<String, String> {
        self.extra_params.as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default()
    }
}

/// The provider as the admin UI renders it — **without** `client_secret`. The list
/// endpoint reaches the browser, and the secret has no business there.
#[derive(Debug, Clone, Serialize)]
pub struct OauthProviderView {
    pub name:              String,
    pub display_name:      String,
    pub auth_url:          String,
    pub token_url:         String,
    pub client_id:         String,
    pub redirect_uri:      String,
    pub extra_params:      Option<String>,
    /// So the admin sees a secret is set without the value crossing the wire.
    pub has_client_secret: bool,
}

impl From<OauthProviderRow> for OauthProviderView {
    fn from(r: OauthProviderRow) -> Self {
        OauthProviderView {
            has_client_secret: !r.client_secret.is_empty(),
            name:              r.name,
            display_name:      r.display_name,
            auth_url:          r.auth_url,
            token_url:         r.token_url,
            client_id:         r.client_id,
            redirect_uri:      r.redirect_uri,
            extra_params:      r.extra_params,
        }
    }
}

const SELECT: &str =
    "SELECT name, display_name, auth_url, token_url, client_id, client_secret, \
            redirect_uri, extra_params, created_at, updated_at \
     FROM oauth_providers";

// ── Reads ────────────────────────────────────────────────────────────────────

pub async fn list(pool: &SqlitePool) -> Result<Vec<OauthProviderRow>> {
    let rows = sqlx::query_as::<_, OauthProviderRow>(sqlx::AssertSqlSafe(format!("{SELECT} ORDER BY name")))
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn get(pool: &SqlitePool, name: &str) -> Result<Option<OauthProviderRow>> {
    let row = sqlx::query_as::<_, OauthProviderRow>(sqlx::AssertSqlSafe(format!("{SELECT} WHERE name = ?")))
        .bind(name)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

// ── Writes ───────────────────────────────────────────────────────────────────

pub struct UpsertProvider<'a> {
    pub name:          &'a str,
    pub display_name:  &'a str,
    pub auth_url:      &'a str,
    pub token_url:     &'a str,
    pub client_id:     &'a str,
    pub client_secret: &'a str,
    pub redirect_uri:  &'a str,
    pub extra_params:  Option<&'a str>,
}

pub async fn upsert(pool: &SqlitePool, p: UpsertProvider<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO oauth_providers
            (name, display_name, auth_url, token_url, client_id, client_secret, redirect_uri, extra_params)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(name) DO UPDATE SET
             display_name  = excluded.display_name,
             auth_url      = excluded.auth_url,
             token_url     = excluded.token_url,
             client_id     = excluded.client_id,
             -- Keep the stored secret when the form submits an empty one: the admin
             -- editing a provider's URLs should not have to re-paste the secret,
             -- which the list view never gave back to the browser.
             client_secret = CASE WHEN excluded.client_secret = ''
                                  THEN oauth_providers.client_secret
                                  ELSE excluded.client_secret END,
             redirect_uri  = excluded.redirect_uri,
             extra_params  = excluded.extra_params,
             updated_at    = datetime('now')",
    )
    .bind(p.name)
    .bind(p.display_name)
    .bind(p.auth_url)
    .bind(p.token_url)
    .bind(p.client_id)
    .bind(p.client_secret)
    .bind(p.redirect_uri)
    .bind(p.extra_params)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn delete(pool: &SqlitePool, name: &str) -> Result<()> {
    sqlx::query("DELETE FROM oauth_providers WHERE name = ?")
        .bind(name)
        .execute(pool)
        .await?;
    Ok(())
}
