//! Shared role-capability gate for API handlers.

use skald_core::db::{role_capabilities, roles::ADMIN_ROLE_ID, users};
use skald_core::skald::Skald;

use super::ApiError;

/// Fails with 403 unless the caller is an admin.
///
/// For instance-wide settings, which are admin-by-construction rather than
/// gated on a named capability: there is no meaningful role that should be able
/// to change the interface language or a background agent's schedule for
/// everybody without also being an admin.
///
/// Needed because the sidebar hiding a page is **not** access control — the
/// endpoints behind Config were reachable by any authenticated session.
pub async fn require_admin(skald: &Skald, user_id: &str) -> Result<(), ApiError> {
    if is_admin(skald, user_id).await? {
        Ok(())
    } else {
        Err(ApiError::forbidden("this setting is admin-only"))
    }
}

/// Whether the caller is an admin. For handlers that serve everyone but reveal
/// more to an admin, rather than refusing outright.
pub async fn is_admin(skald: &Skald, user_id: &str) -> Result<bool, ApiError> {
    let user = users::get(skald.db(), user_id)
        .await?
        .ok_or_else(|| ApiError::unauthorized("unknown user"))?;
    Ok(user.role_id == ADMIN_ROLE_ID)
}

/// Fails with 403 unless the caller's role holds `cap` (admin holds everything).
pub async fn require_cap(skald: &Skald, user_id: &str, cap: &str) -> Result<(), ApiError> {
    let user = skald_core::db::users::get(skald.db(), user_id).await?
        .ok_or_else(|| ApiError::unauthorized("unknown user"))?;
    if role_capabilities::has(skald.db(), &user.role_id, cap).await? {
        Ok(())
    } else {
        Err(ApiError::forbidden(format!("your role lacks the capability `{cap}`")))
    }
}
