use std::sync::Arc;

use axum::{Json, extract::{Path, State}};
use serde::{Deserialize, Serialize};

use skald_core::db::users::UserSummary;
use skald_core::skald::Skald;

use super::ApiError;

// ── GET /api/users ───────────────────────────────────────────────────────────

pub async fn list(State(skald): State<Arc<Skald>>) -> Result<Json<Vec<UserSummary>>, ApiError> {
    let users = skald.users().list().await?;
    Ok(Json(users))
}

// ── POST /api/users ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateUserBody {
    pub username:    String,
    pub display_name: Option<String>,
    pub role_id:      String,
    pub password:     String,
    #[serde(default)]
    pub encrypted:    bool,
    #[serde(default)]
    pub birthdate:    Option<String>,
    #[serde(default)]
    pub sex:          Option<String>,
    #[serde(default)]
    pub notes:        Option<String>,
}

#[derive(Serialize)]
pub struct CreatedUser {
    pub id: String,
}

/// Empty/whitespace strings normalize to `None` (the form clears a field by
/// blanking it), and the surviving values are validated: `birthdate` must be a
/// real ISO `YYYY-MM-DD` date, not in the future; the free-text fields are
/// length-capped so the prompt block stays sane.
fn normalize_profile_fields(
    birthdate: Option<&str>,
    sex:       Option<&str>,
    notes:     Option<&str>,
) -> Result<(Option<String>, Option<String>, Option<String>), ApiError> {
    let clean = |s: Option<&str>| s.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);

    let birthdate = clean(birthdate);
    if let Some(b) = &birthdate {
        let dob = chrono::NaiveDate::parse_from_str(b, "%Y-%m-%d")
            .map_err(|_| ApiError::bad_request("birthdate must be a YYYY-MM-DD date"))?;
        if dob > chrono::Utc::now().date_naive() {
            return Err(ApiError::bad_request("birthdate cannot be in the future"));
        }
    }
    let sex = clean(sex);
    if sex.as_deref().is_some_and(|s| s.len() > 50) {
        return Err(ApiError::bad_request("sex is too long (max 50 chars)"));
    }
    let notes = clean(notes);
    if notes.as_deref().is_some_and(|s| s.len() > 2000) {
        return Err(ApiError::bad_request("notes are too long (max 2000 chars)"));
    }
    Ok((birthdate, sex, notes))
}

pub async fn create(
    State(skald): State<Arc<Skald>>,
    Json(body): Json<CreateUserBody>,
) -> Result<Json<CreatedUser>, ApiError> {
    let username = body.username.trim();
    if username.is_empty() {
        return Err(ApiError::bad_request("username must not be empty"));
    }
    if body.password.is_empty() {
        return Err(ApiError::bad_request("password must not be empty"));
    }
    let (birthdate, sex, notes) = normalize_profile_fields(
        body.birthdate.as_deref(),
        body.sex.as_deref(),
        body.notes.as_deref(),
    )?;
    let id = skald
        .users()
        .register_user(username, body.display_name.as_deref(), &body.role_id, Some(&body.password), body.encrypted)
        .await?;

    // Directory profile fields are not part of registration — set them in a
    // follow-up write (keeps `UserManager::register_user`'s signature stable).
    if birthdate.is_some() || sex.is_some() || notes.is_some() {
        skald_core::db::users::set_directory_fields(
            skald.db(),
            &id,
            birthdate.as_deref(),
            sex.as_deref(),
            notes.as_deref(),
        )
        .await?;
    }

    // Provision the user's container now (blueprint §6). Best-effort: a failure here
    // is not fatal to user creation — boot reconciliation will retry.
    if let Err(e) = skald.container().ensure(&id).await {
        tracing::warn!(user = %id, error = %e, "failed to provision user container (will retry at next boot)");
    }

    Ok(Json(CreatedUser { id }))
}

// ── PUT /api/users/:id ───────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct UpdateUserBody {
    pub username:     String,
    pub display_name: Option<String>,
    pub role_id:      String,
    #[serde(default)]
    pub active:       bool,
    #[serde(default)]
    pub birthdate:    Option<String>,
    #[serde(default)]
    pub sex:          Option<String>,
    #[serde(default)]
    pub notes:        Option<String>,
}

pub async fn update(
    State(skald): State<Arc<Skald>>,
    Path(id): Path<String>,
    Json(body): Json<UpdateUserBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let username = body.username.trim();
    if username.is_empty() {
        return Err(ApiError::bad_request("username must not be empty"));
    }
    let (birthdate, sex, notes) = normalize_profile_fields(
        body.birthdate.as_deref(),
        body.sex.as_deref(),
        body.notes.as_deref(),
    )?;
    skald_core::db::users::update_profile(
        skald.db(),
        &id,
        username,
        body.display_name.as_deref(),
        &body.role_id,
    )
    .await?;

    // active is separate because it's a boolean flip
    skald_core::db::users::set_active(skald.db(), &id, body.active).await?;

    // Directory profile fields are a separate write too (same shape as active).
    skald_core::db::users::set_directory_fields(
        skald.db(),
        &id,
        birthdate.as_deref(),
        sex.as_deref(),
        notes.as_deref(),
    )
    .await?;

    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── DELETE /api/users/:id ────────────────────────────────────────────────────

pub async fn delete(
    State(skald): State<Arc<Skald>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    skald.users().delete_user(&id).await?;
    // Tear down the user's container (best-effort; a missing one is fine).
    if let Err(e) = skald.container().remove(&id).await {
        tracing::warn!(user = %id, error = %e, "failed to remove user container");
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

// ── POST /api/users/:id/password ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ResetPasswordBody {
    pub password: String,
}

pub async fn reset_password(
    State(skald): State<Arc<Skald>>,
    Path(id): Path<String>,
    Json(body): Json<ResetPasswordBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if body.password.is_empty() {
        return Err(ApiError::bad_request("password must not be empty"));
    }
    // change_password with no old password and a new one resets it. For
    // encrypted users the admin must supply the old password; this endpoint
    // is for cleartext users or for admin-initiated resets of cleartext
    // accounts. For encrypted users, the user should change their own password.
    skald.users().change_password(&id, None, Some(&body.password)).await
        .map_err(|e| ApiError::bad_request(e.to_string()))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
