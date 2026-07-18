use std::sync::Arc;

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};

use skald_core::skald::Skald;

use super::ApiError;

// ── GET /api/setup/status ────────────────────────────────────────────────────
//
// `needs_setup` is true when no user has ever been created. The frontend uses
// this to decide whether to show the first-run setup screen.

#[derive(Serialize)]
pub struct SetupStatus {
    pub needs_setup: bool,
}

pub async fn status(State(skald): State<Arc<Skald>>) -> Result<Json<SetupStatus>, ApiError> {
    let count = skald.users().count().await?;
    Ok(Json(SetupStatus { needs_setup: count == 0 }))
}

// ── POST /api/setup/user — create the first (admin) user ────────────────────

#[derive(Deserialize)]
pub struct CreateUserBody {
    pub username:  String,
    pub password:  String,
    #[serde(default)]
    pub encrypted: bool,
    /// Chosen interface language — becomes the instance default (`ui_locale`).
    #[serde(default)]
    pub locale:    Option<String>,
}

#[derive(Serialize)]
pub struct CreateUserResult {
    pub user_id: String,
}

pub async fn create_user(
    State(skald): State<Arc<Skald>>,
    Json(body): Json<CreateUserBody>,
) -> Result<Json<CreateUserResult>, ApiError> {
    // Guard: the setup endpoint is only available before the first user exists.
    let count = skald.users().count().await?;
    if count > 0 {
        return Err(ApiError::bad_request("setup is already complete"));
    }

    let username = body.username.trim();
    if username.is_empty() {
        return Err(ApiError::bad_request("username must not be empty"));
    }
    if body.password.is_empty() {
        return Err(ApiError::bad_request("password must not be empty"));
    }
    let locale = body.locale.as_deref().map(str::trim).filter(|s| !s.is_empty());
    if let Some(l) = locale {
        if !skald_core::i18n::is_supported(l) {
            return Err(ApiError::bad_request("unsupported locale"));
        }
    }

    let id = skald
        .users()
        .register_user(username, None, "admin", Some(&body.password), body.encrypted)
        .await?;

    // The first-run language choice is instance-wide: it lands in the registry
    // config as the default every user follows until they override it.
    if let Some(l) = locale {
        skald.config().set(skald_core::i18n::DEFAULT_LOCALE_KEY, l).await?;
    }

    Ok(Json(CreateUserResult { user_id: id }))
}
