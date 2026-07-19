//! Capability grants per role (blueprint §14).
//!
//! Registry table in `system.db`. Authorization is a **capability on the role**,
//! not `if role == admin` (§0.1). The MCP registration axis (§14):
//!
//! - [`REGISTER_REMOTE`] / [`REGISTER_LOCAL_FROM_CATALOG`] — self-service, any user
//!   (egress-only / already-vetted code).
//! - [`REGISTER_LOCAL_SCRIPT`] / [`MANAGE_CATALOG`] — admin-only (RCE / catalog
//!   curation).
//!
//! The built-in `admin` role implicitly holds every capability — [`has`] short-
//! circuits on it — so only non-admin roles ever need rows here.

use anyhow::Result;
use sqlx::SqlitePool;

use super::roles::ADMIN_ROLE_ID;

/// Register a remote MCP into one's own scope (egress-only, self-service).
pub const REGISTER_REMOTE: &str = "mcp.register_remote";
/// Instantiate an admin-vetted local-script connector from the catalog.
pub const REGISTER_LOCAL_FROM_CATALOG: &str = "mcp.register_local_from_catalog";
/// Add a brand-new local script to the catalog (RCE surface — admin only).
pub const REGISTER_LOCAL_SCRIPT: &str = "mcp.register_local_script";
/// Curate the connector catalog (admin only).
pub const MANAGE_CATALOG: &str = "mcp.manage_catalog";

/// Manage shared on-disk folders — create/describe/delete and grant membership
/// (blueprint §6). Admin-only for now; not in [`DEFAULT_USER_CAPABILITIES`], so
/// `admin` holds it implicitly (via [`has`]) and opening it to another role later
/// is a single [`grant`], no code change.
pub const MANAGE_SHARED_FOLDERS: &str = "folders.manage";

/// Enable/disable plugins, edit their instance-wide config and grant per-user
/// access (the `plugin_access` table). Admin-only for now — same implicit-hold
/// pattern as [`MANAGE_SHARED_FOLDERS`].
pub const MANAGE_PLUGINS: &str = "plugin.manage";

/// The default capabilities of an ordinary (non-admin) user role.
pub const DEFAULT_USER_CAPABILITIES: &[&str] = &[REGISTER_REMOTE, REGISTER_LOCAL_FROM_CATALOG];

// ── Reads ────────────────────────────────────────────────────────────────────

/// Whether a role holds a capability. `admin` holds everything by construction.
pub async fn has(pool: &SqlitePool, role_id: &str, capability: &str) -> Result<bool> {
    if role_id == ADMIN_ROLE_ID {
        return Ok(true);
    }
    let row = sqlx::query_as::<_, (i64,)>(
        "SELECT 1 FROM role_capabilities WHERE role_id = ? AND capability = ?",
    )
    .bind(role_id)
    .bind(capability)
    .fetch_optional(pool)
    .await?;
    Ok(row.is_some())
}

pub async fn list_for_role(pool: &SqlitePool, role_id: &str) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT capability FROM role_capabilities WHERE role_id = ? ORDER BY capability",
    )
    .bind(role_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(c,)| c).collect())
}

// ── Writes ───────────────────────────────────────────────────────────────────

pub async fn grant(pool: &SqlitePool, role_id: &str, capability: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO role_capabilities (role_id, capability) VALUES (?, ?)")
        .bind(role_id)
        .bind(capability)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn revoke(pool: &SqlitePool, role_id: &str, capability: &str) -> Result<()> {
    sqlx::query("DELETE FROM role_capabilities WHERE role_id = ? AND capability = ?")
        .bind(role_id)
        .bind(capability)
        .execute(pool)
        .await?;
    Ok(())
}

/// Seeds the standard non-admin capability set for a newly created role.
/// Idempotent. No-op for `admin` (which holds everything implicitly).
pub async fn seed_defaults(pool: &SqlitePool, role_id: &str) -> Result<()> {
    if role_id == ADMIN_ROLE_ID {
        return Ok(());
    }
    for cap in DEFAULT_USER_CAPABILITIES {
        grant(pool, role_id, cap).await?;
    }
    Ok(())
}
