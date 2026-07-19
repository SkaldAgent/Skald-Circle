//! Authentication gate for `/api` — deny by default.
//!
//! Applied as a layer over the whole API router, so **every** endpoint requires
//! a valid session cookie except the handful named in [`is_public`]. A new route
//! is therefore protected the moment it is added, with no extra step; opening it
//! up is the deliberate act.
//!
//! Kept separate from the `auth` handlers on purpose: this is the cross-cutting
//! gate, they are the login/logout endpoints. On a hit it resolves the session
//! once and injects [`AuthUser`] into the request, so downstream handlers — and
//! the per-user pool routing that will land with the multi-user split — read the
//! identity without re-parsing the cookie.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};

use skald_core::auth::COOKIE_NAME;
use skald_core::skald::Skald;

/// The authenticated user, injected into request extensions by [`require_auth`]
/// for every gated endpoint. This is where per-user pool routing will read the
/// id once the database split (blueprint §5.1) reaches the call sites.
#[derive(Clone, Debug)]
pub struct AuthUser {
    pub user_id: String,
}

/// Endpoints reachable without a session. Everything else under `/api` needs a
/// valid cookie.
///
/// - `auth/login` — the bootstrap: you cannot have a session before it.
/// - `auth/logout` — idempotent; tolerating an already-expired session is kinder
///   than answering 401 to someone trying to log out.
/// - `auth/me` — the canonical "am I logged in?" probe; it answers 401 itself.
/// - `setup/status` / `setup/user` — called before the first user exists, and
///   `create_user` already refuses once one does.
fn is_public(path: &str) -> bool {
    // The layer sits on the inner router, so the path arrives without the `/api`
    // nest prefix — but strip it defensively so a move of the layer can't
    // silently open everything up.
    let p = path.strip_prefix("/api").unwrap_or(path);
    matches!(
        p,
        "/auth/login" | "/auth/logout" | "/auth/me" | "/setup/status" | "/setup/user"
    )
}

/// Extracts the session token from the `Cookie` header.
fn session_token(headers: &axum::http::HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|pair| pair.trim().strip_prefix(&format!("{COOKIE_NAME}=")))
        .next()
        .map(str::to_owned)
}

/// The gate. Public paths pass through untouched; every other request must carry
/// a cookie that maps to a live session, or it gets 401 before reaching a
/// handler.
pub async fn require_auth(
    State(skald): State<Arc<Skald>>,
    mut req: Request,
    next: Next,
) -> Response {
    if is_public(req.uri().path()) {
        return next.run(req).await;
    }

    match session_token(req.headers()).and_then(|t| skald.sessions().user_of(&t)) {
        Some(user_id) => {
            // `AuthUser` is the bin-private identity for host handlers; `Caller`
            // is the core-api mirror so plugin routers (which cannot name
            // bin-crate types) can identify the caller too.
            req.extensions_mut().insert(core_api::plugin::Caller { user_id: user_id.clone() });
            req.extensions_mut().insert(AuthUser { user_id });
            next.run(req).await
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Enabled-gate for plugin-contributed routers (`/api/plugin/<id>/…`).
///
/// Plugin routers are all mounted at boot — including those of disabled
/// plugins, whose `http_router()` must be safe to build before `start`. This
/// gate re-checks the enabled flag in the DB on **every** request, so a
/// disabled plugin answers 404 (it does not reveal it exists) and enabling one
/// at runtime serves its routes immediately, with no restart. Runs inside
/// `require_auth`, so by this point the caller is a logged-in user.
pub async fn plugin_enabled_gate(
    State((skald, plugin_id)): State<(Arc<Skald>, String)>,
    req: Request,
    next: Next,
) -> Response {
    match skald.plugin_manager().is_enabled(&plugin_id).await {
        Ok(true) => next.run(req).await,
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::warn!(plugin = %plugin_id, error = %e, "plugin enabled-gate: DB read failed");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}
