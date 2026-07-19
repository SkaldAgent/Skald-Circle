//! Shared role-capability gate for API handlers.

use skald_core::db::role_capabilities;
use skald_core::skald::Skald;

use super::ApiError;

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
