use std::sync::Arc;

use axum::{Json, extract::State};
use core_api::system_bus::SystemEvent;
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

// ── GET /api/setup/profiles — seed profiles offered by the picker ────────────

#[derive(Serialize)]
pub struct SeedProfileInfo {
    pub id:    String,
    pub label: String,
}

/// The seed profiles the first-run picker offers (§0.1: the neutral mechanism,
/// domain flavour in the data). Pre-auth, so it is on the setup allowlist.
pub async fn profiles() -> Json<Vec<SeedProfileInfo>> {
    let list = skald_core::setup::seed_profiles()
        .into_iter()
        .map(|p| SeedProfileInfo { id: p.id.to_string(), label: p.label.to_string() })
        .collect();
    Json(list)
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
    /// Chosen seed profile id. Defaults to the first shipped profile.
    #[serde(default)]
    pub profile:   Option<String>,
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
    let profile = body
        .profile
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("family");
    if skald_core::setup::seed_profile(profile).is_none() {
        return Err(ApiError::bad_request("unknown seed profile"));
    }

    // The shared first-run seam: seed the profile's roles, create the admin, set
    // the default locale — the same path skald-setup takes, so the two never drift.
    let id = skald_core::setup::initialize_instance(
        skald.users(),
        skald.db(),
        profile,
        skald_core::setup::FirstAdmin {
            username,
            display_name: None,
            password: Some(&body.password),
            encrypted: body.encrypted,
            locale,
        },
    )
    .await?;

    // The web wizard runs against a *live* server, where boot reconciliation has
    // already happened — so without this the first admin had no container until the
    // next restart. One announcement, and the reconciler provisions it like any
    // other user (blueprint §6). The console shell needs no equivalent: it runs
    // before the server, and `reconcile_all()` picks the admin up at boot.
    skald.system_bus().send(SystemEvent::UserCreated { user_id: id.clone() });

    Ok(Json(CreateUserResult { user_id: id }))
}
