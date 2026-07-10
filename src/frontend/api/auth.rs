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
    pub username:     String,
    pub display_name: Option<String>,
    pub role_id:      String,
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

    Ok(Json(MeResponse {
        username:     user.username,
        display_name: user.display_name,
        role_id:      user.role_id,
    })
    .into_response())
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

// ── PUT /api/auth/profile — update display name ──────────────────────────────

#[derive(Deserialize)]
pub struct UpdateProfileBody {
    pub display_name: Option<String>,
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

    skald_core::db::users::rename(
        skald.db(),
        &auth.user_id,
        &user.username,
        body.display_name.as_deref(),
    )
    .await?;

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
