use std::sync::Arc;

use axum::{
    Json,
    extract::Extension,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use skald_core::auth::COOKIE_NAME;
use skald_core::skald::Skald;

use super::guard::AuthUser;
use super::ApiError;

// ── POST /api/auth/login ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LoginBody {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResult {
    pub ok: bool,
}

/// Authenticates the user, unlocks their database (pool stays open until
/// restart), mints a session token, and sets it as an `HttpOnly` cookie.
pub async fn login(
    State(skald): State<Arc<Skald>>,
    Json(body): Json<LoginBody>,
) -> Result<Response, ApiError> {
    let token = skald
        .sessions()
        .login(&body.username, &body.password)
        .await
        .map_err(|_| {
            ApiError::bad_request("Invalid username or password")
        })?;

    let cookie = format!(
        "{COOKIE_NAME}={token}; HttpOnly; Path=/; SameSite=Strict; Max-Age=2592000"
    );

    Ok((
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(LoginResult { ok: true }),
    )
        .into_response())
}

// ── GET /api/auth/me ─────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct MeResponse {
    pub username:       String,
    pub display_name:   Option<String>,
    pub role_id:        String,
    /// Interface mode resolved from the role's `attrs.ui_mode` — "full" unless
    /// the role opts into the simplified UI. Never hardcoded per-role: it is
    /// data on the role row (§0.1), and `admin` is always "full".
    pub ui_mode:        String,
    /// The user's own locale override (NULL = follow the instance default).
    pub locale:         Option<String>,
    /// Whether the user's database is encrypted (drives the profile UI).
    pub encrypted:      bool,
    /// Instance default locale (registry config `ui_locale`).
    pub default_locale: String,
}

/// Returns the authenticated user's profile, or 401 if no valid session.
pub async fn me(
    State(skald): State<Arc<Skald>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let token = match extract_session_token(&headers) {
        Some(t) => t,
        None => return Ok(StatusCode::UNAUTHORIZED.into_response()),
    };

    let user_id = match skald.sessions().user_of(&token) {
        Some(id) => id,
        None => return Ok(StatusCode::UNAUTHORIZED.into_response()),
    };

    let user = skald
        .users()
        .get(&user_id)
        .await?
        .ok_or_else(|| ApiError::not_found("user not found"))?;

    let ui_mode = resolve_ui_mode(&skald, &user.role_id).await;
    let default_locale = skald
        .config()
        .get(skald_core::i18n::DEFAULT_LOCALE_KEY)
        .await?
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "en".into());

    Ok(Json(MeResponse {
        username:     user.username,
        display_name: user.display_name,
        role_id:      user.role_id,
        ui_mode,
        locale:       user.locale,
        encrypted:    user.encrypted,
        default_locale,
    })
    .into_response())
}

/// Reads `roles.attrs.ui_mode` for the given role. Any error or missing key
/// resolves to "full" — the simplified UI is strictly opt-in.
async fn resolve_ui_mode(skald: &Skald, role_id: &str) -> String {
    if role_id == skald_core::db::roles::ADMIN_ROLE_ID {
        return "full".into();
    }
    let attrs = skald_core::db::roles::get(skald.db(), role_id)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.attrs);
    attrs
        .and_then(|a| serde_json::from_str::<serde_json::Value>(&a).ok())
        .and_then(|v| v.get("ui_mode")?.as_str().map(str::to_owned))
        .filter(|m| m == "simple" || m == "full")
        .unwrap_or_else(|| "full".into())
}

// ── POST /api/auth/logout ────────────────────────────────────────────────────

pub async fn logout(
    State(skald): State<Arc<Skald>>,
    headers: HeaderMap,
) -> Response {
    if let Some(token) = extract_session_token(&headers) {
        skald.sessions().logout(&token);
    }
    // Clear the cookie.
    let expired = format!(
        "{COOKIE_NAME}=; HttpOnly; Path=/; SameSite=Strict; Max-Age=0"
    );
    (
        StatusCode::OK,
        [(header::SET_COOKIE, expired)],
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Parses the `Cookie` header and extracts the session token.
fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in raw.split(';') {
        let pair = pair.trim();
        if let Some(val) = pair.strip_prefix(&format!("{COOKIE_NAME}=")) {
            return Some(val.to_string());
        }
    }
    None
}

// ── PUT /api/auth/profile — update display name / locale ─────────────────────

// Tri-state fields: absent = don't touch, `null` = clear, value = set. Serde
// maps them onto `Option<Option<T>>` with `#[serde(default)]`.
#[derive(Deserialize)]
pub struct UpdateProfileBody {
    #[serde(default)]
    pub display_name: Option<Option<String>>,
    #[serde(default)]
    pub locale:       Option<Option<String>>,
}

pub async fn update_profile(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<UpdateProfileBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let user = skald
        .users()
        .get(&auth.user_id)
        .await?
        .ok_or_else(|| ApiError::not_found("user not found"))?;

    if let Some(display_name) = body.display_name {
        skald_core::db::users::rename(
            skald.db(),
            &auth.user_id,
            &user.username,
            display_name.as_deref().filter(|s| !s.trim().is_empty()),
        )
        .await?;
    }

    if let Some(locale) = body.locale {
        match locale.as_deref().map(str::trim) {
            None | Some("") => {
                skald_core::db::users::set_locale(skald.db(), &auth.user_id, None).await?;
            }
            Some(l) if skald_core::i18n::is_supported(l) => {
                skald_core::db::users::set_locale(skald.db(), &auth.user_id, Some(l)).await?;
            }
            Some(_) => return Err(ApiError::bad_request("unsupported locale")),
        }
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── POST /api/auth/change-password ───────────────────────────────────────────

#[derive(Deserialize)]
pub struct ChangePasswordBody {
    pub current_password: Option<String>,
    pub new_password:     String,
}

pub async fn change_password(
    State(skald): State<Arc<Skald>>,
    Extension(auth): Extension<AuthUser>,
    Json(body): Json<ChangePasswordBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if body.new_password.is_empty() {
        return Err(ApiError::bad_request("new password must not be empty"));
    }

    skald
        .users()
        .change_password(
            &auth.user_id,
            body.current_password.as_deref(),
            Some(&body.new_password),
        )
        .await
        .map_err(|e| match e {
            skald_core::users::AuthError::WrongPassword => {
                ApiError::bad_request("Current password is incorrect")
            }
            other => ApiError::bad_request(other.to_string()),
        })?;

    Ok(Json(serde_json::json!({ "ok": true })))
}
