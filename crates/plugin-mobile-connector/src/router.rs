//! The plugin's HTTP surface, mounted by the main `WebFrontend` under
//! `/api/plugin/mobile-connector/` behind Skald's normal auth + enabled-gate.
//!
//! Two audiences on one router:
//! - the **QR endpoint** (`/pairingqrcode`) — renders the pairing QR PNG on
//!   demand from the in-memory session (no QR ever touches disk);
//! - the **Mobile App console** — the JSON API + the page fragment
//!   (`web/app.js`) behind the single "Mobile App" menu page: connection
//!   status, device list, self-service pairing, and device revocation.
//!
//! Every request resolves the *current* [`RelayApp`] through the shared state
//! cell (`Arc<Mutex<Option<Arc<RelayApp>>>>`), so a reconfigure (reload → fresh
//! `RelayApp`) is transparent. Access is self-scoped per caller: any logged-in
//! user may pair a device (it auto-binds to them), list their own devices and
//! revoke them; listing every device and (re)binding to another user stays
//! admin-only (gated on [`UserChannelApi::is_admin`]).

use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use core_api::plugin::Caller;
use skald_relay_client::{ClientState, SessionState};

use crate::app::RelayApp;
use crate::PLUGIN_ID;

/// Shared cell type: an `Arc` to a `Mutex` holding the (optional) live app.
/// Cloned cheaply and safely shared between the plugin and the router.
type StateCell = Arc<Mutex<Option<Arc<RelayApp>>>>;

// Namespaced i18n keys for the router's user-facing strings (backend tables in
// `../i18n/*.json`). Resolved to the caller's language via `app.i18n()`.
const KEY_RELAY_NOT_CONNECTED: &str = "plugin.mobile-connector.err.relay_not_connected";
const KEY_ADMIN_ONLY:          &str = "plugin.mobile-connector.err.admin_only";
const KEY_USER_ID_EMPTY:       &str = "plugin.mobile-connector.err.user_id_empty";
const KEY_PUBKEY_HEX:          &str = "plugin.mobile-connector.err.pubkey_hex";
const KEY_NOT_DEVICE_OWNER:    &str = "plugin.mobile-connector.err.not_device_owner";
const KEY_NOT_PAIRING_OWNER:   &str = "plugin.mobile-connector.err.not_pairing_owner";

/// Build the plugin's router. Takes the shared state cell so each request
/// resolves the *current* `RelayApp` — not a snapshot from startup.
pub fn build(state_cell: StateCell) -> Router {
    Router::new()
        .route("/pairingqrcode", get(pairing_qr))
        // Page fragments (served as ES modules to the browser).
        .route("/web/app.js",    get(|| async { serve_js(include_str!("../web/app.js")) }))
        .route("/web/common.js", get(|| async { serve_js(include_str!("../web/common.js")) }))
        .route("/web/i18n.js",   get(|| async { serve_js(include_str!("../web/i18n.js")) }))
        // Mobile App console API.
        .route("/status", get(status))
        .route("/pairing", post(start_pairing).delete(stop_pairing))
        .route("/devices", get(list_devices))
        .route("/devices/bind", post(bind_device))
        .route("/devices/revoke", post(revoke_device))
        .with_state(state_cell)
}

// ── Console: shared plumbing ──────────────────────────────────────────────────

/// Resolve the live app, or `503` when the plugin is enabled but its runloop is
/// not up (e.g. no `relay_url` configured).
async fn app_or_503(cell: &StateCell) -> Result<Arc<RelayApp>, Response> {
    cell.lock().await.as_ref().map(Arc::clone).ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "mobile connector is not running").into_response()
    })
}

/// Fail-closed admin gate, via [`UserChannelApi::is_admin`].
async fn require_admin(app: &RelayApp, caller: &Caller) -> Result<(), Response> {
    if app.user_channel.is_admin(&caller.user_id).await {
        Ok(())
    } else {
        let msg = app.i18n().for_user(&caller.user_id, KEY_ADMIN_ONLY, &[]).await;
        Err((StatusCode::FORBIDDEN, msg).into_response())
    }
}

/// Resolve the app and check admin in one step.
async fn admin_app(cell: &StateCell, caller: &Caller) -> Result<Arc<RelayApp>, Response> {
    let app = app_or_503(cell).await?;
    require_admin(&app, caller).await?;
    Ok(app)
}

fn bad_request(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, msg.into()).into_response()
}

async fn decode_pubkey(app: &RelayApp, caller: &Caller, hex: &str) -> Result<[u8; 32], Response> {
    match skald_relay_common::crypto::decode_hex::<32>(hex) {
        Some(pk) => Ok(pk),
        None => {
            let msg = app.i18n().for_user(&caller.user_id, KEY_PUBKEY_HEX, &[]).await;
            Err(bad_request(msg))
        }
    }
}

// ── GET /status ───────────────────────────────────────────────────────────────

/// Connection status for the page header. Works also when the runloop is down
/// (no `relay_url` yet) so the page can render the not-running state.
async fn status(State(cell): State<StateCell>) -> Response {
    match cell.lock().await.as_ref() {
        Some(app) => Json(json!({
            "running":    true,
            "connected":  app.client().is_connected(),
            "relay_url":  app.relay_url(),
            "last_error": app.client().last_error(),
        }))
        .into_response(),
        None => Json(json!({
            "running":    false,
            "connected":  false,
            "relay_url":  null,
            "last_error": null,
        }))
        .into_response(),
    }
}

// ── POST/DELETE /pairing ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StartPairingBody {
    /// Window lifetime in seconds; `0`/absent = the configured default, capped at 600.
    #[serde(default)]
    ttl: Option<u32>,
}

/// Open a pairing window and return the QR URL. Self-service: the caller
/// becomes the pending owner, so a device that pairs in this window
/// auto-binds to them.
async fn start_pairing(
    State(cell): State<StateCell>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<StartPairingBody>,
) -> Response {
    let app = match app_or_503(&cell).await { Ok(a) => a, Err(r) => return r };
    // Pairing brokers through the relay: without a live WS there is no channel to
    // send `pairing_start` on ("WS outbound channel closed"). Fail with an
    // actionable message instead of the transport-level one.
    if !app.client().is_connected() {
        let msg = app.i18n().for_user(&caller.user_id, KEY_RELAY_NOT_CONNECTED, &[]).await;
        return (StatusCode::SERVICE_UNAVAILABLE, msg).into_response();
    }
    let ttl = body.ttl.unwrap_or(0).min(600);
    app.set_pending_owner(Some(caller.user_id.clone())).await;
    match app.client().start_pairing(ttl).await {
        Ok(started) => Json(json!({
            "url":        format!("/api/plugin/{PLUGIN_ID}/pairingqrcode?code={}", started.code),
            "code":       started.code,
            "expires_at": started.expires_at,
        }))
        .into_response(),
        Err(e) => {
            app.set_pending_owner(None).await;
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
        }
    }
}

/// Close the pairing window and disarm auto-binding. Only the user who opened
/// the window (or an admin) may close it.
async fn stop_pairing(
    State(cell): State<StateCell>,
    Extension(caller): Extension<Caller>,
) -> Response {
    let app = match app_or_503(&cell).await { Ok(a) => a, Err(r) => return r };
    let owner = app.pending_owner().await;
    if owner.as_deref() != Some(caller.user_id.as_str())
        && !app.user_channel.is_admin(&caller.user_id).await
    {
        let msg = app.i18n().for_user(&caller.user_id, KEY_NOT_PAIRING_OWNER, &[]).await;
        return (StatusCode::FORBIDDEN, msg).into_response();
    }
    app.set_pending_owner(None).await;
    match app.client().stop_pairing().await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── GET /devices ────────────────────────────────────────────────────────────────

/// List devices, each tagged with its bound user, state and metadata. An admin
/// sees every known device; anyone else only the devices bound to them.
async fn list_devices(
    State(cell): State<StateCell>,
    Extension(caller): Extension<Caller>,
) -> Response {
    let app = match app_or_503(&cell).await { Ok(a) => a, Err(r) => return r };
    let is_admin = app.user_channel.is_admin(&caller.user_id).await;
    let rows = app.client().list_clients().await;
    let bindings = app.bindings.read().await;
    let devices: Vec<Value> = rows
        .into_iter()
        .filter_map(|r| {
            let pk_hex = hex::encode(r.ed25519_pub);
            let bound_user = bindings.user_for_pubkey(&pk_hex);
            if !is_admin && bound_user.as_deref() != Some(caller.user_id.as_str()) {
                return None;
            }
            let device_info: Option<Value> =
                r.device_info.as_deref().and_then(|s| serde_json::from_str(s).ok());
            Some(json!({
                "pubkey":      pk_hex,
                "state":       if r.state == ClientState::Authorized { "authorized" } else { "pending" },
                "bound_user":  bound_user,
                "platform":    r.platform,
                "device_info": device_info,
                "last_seen":   r.last_seen,
            }))
        })
        .collect();
    Json(json!({ "devices": devices, "is_admin": is_admin })).into_response()
}

// ── POST /devices/bind + /devices/revoke ────────────────────────────────────────

#[derive(Deserialize)]
struct BindBody {
    pubkey: String,
    user_id: String,
    #[serde(default)]
    display: Option<String>,
}

/// Bind (or reassign) a device to a user and authorize it. Admin-only: users
/// get their devices bound through the self-service pairing window instead.
async fn bind_device(
    State(cell): State<StateCell>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<BindBody>,
) -> Response {
    let app = match admin_app(&cell, &caller).await { Ok(a) => a, Err(r) => return r };
    let pk = match decode_pubkey(&app, &caller, &body.pubkey).await { Ok(p) => p, Err(r) => return r };
    if body.user_id.trim().is_empty() {
        return bad_request(app.i18n().for_user(&caller.user_id, KEY_USER_ID_EMPTY, &[]).await);
    }
    match app.bind_device(pk, body.user_id, body.display).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct RevokeBody {
    pubkey: String,
}

/// Revoke a device and drop its binding. An admin revokes any device; anyone
/// else only a device bound to themselves.
async fn revoke_device(
    State(cell): State<StateCell>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<RevokeBody>,
) -> Response {
    let app = match app_or_503(&cell).await { Ok(a) => a, Err(r) => return r };
    let pk = match decode_pubkey(&app, &caller, &body.pubkey).await { Ok(p) => p, Err(r) => return r };
    let bound = app.bindings.read().await.user_for_pubkey(&body.pubkey);
    if bound.as_deref() != Some(caller.user_id.as_str())
        && !app.user_channel.is_admin(&caller.user_id).await
    {
        let msg = app.i18n().for_user(&caller.user_id, KEY_NOT_DEVICE_OWNER, &[]).await;
        return (StatusCode::FORBIDDEN, msg).into_response();
    }
    match app.revoke_device(pk).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── Static fragment serving ─────────────────────────────────────────────────────

/// Serve an embedded ES module as `text/javascript`. The shell already adds
/// `Cache-Control: no-cache`, so a rebuilt fragment is never served stale.
fn serve_js(body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], body).into_response()
}

// ── QR endpoint (unchanged) ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct QrQuery {
    code: Option<String>,
}

/// `GET /pairingqrcode?code=<random>` → PNG of the QR while active, else a
/// placeholder PNG.
async fn pairing_qr(
    State(cell): State<StateCell>,
    Query(q): Query<QrQuery>,
) -> impl IntoResponse {
    let Some(code) = q.code else {
        return png_response(render_placeholder("QR non valido"));
    };

    let app = match cell.lock().await.as_ref() {
        Some(s) => Arc::clone(s),
        None => return png_response(render_placeholder("Plugin non attivo")),
    };

    match app.client().lookup_pairing(&code) {
        Some((qr, SessionState::Active)) => match serde_json::to_string(&qr) {
            Ok(json) => match render_qr(&json) {
                Ok(png) => png_response(png),
                Err(_) => png_response(render_placeholder("QR error")),
            },
            Err(_) => png_response(render_placeholder("QR error")),
        },
        Some((_, SessionState::Consumed)) => png_response(render_placeholder("QR already used")),
        Some((_, SessionState::Superseded)) => png_response(render_placeholder("QR expired")),
        None => png_response(render_placeholder("QR expired")),
    }
}

/// Wrap PNG bytes in a no-cache image response.
fn png_response(png: Vec<u8>) -> axum::response::Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        png,
    )
        .into_response()
}

/// Render `payload` as a QR PNG (qrcode + image, all in memory).
fn render_qr(payload: &str) -> anyhow::Result<Vec<u8>> {
    use image::{ImageFormat, Luma};
    let code = qrcode::QrCode::new(payload.as_bytes())?;
    let img = code.render::<Luma<u8>>().min_dimensions(512, 512).build();
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, ImageFormat::Png)?;
    Ok(buf.into_inner())
}

/// Render a simple placeholder PNG carrying `msg` as a small QR (renders text so
/// a browser shows *something*; no disk I/O). Falls back to a blank image if the
/// text encode fails.
fn render_placeholder(msg: &str) -> Vec<u8> {
    render_qr(msg).unwrap_or_else(|_| blank_png())
}

/// 1×1 white PNG, used only if QR rendering itself fails.
fn blank_png() -> Vec<u8> {
    use image::{ImageBuffer, ImageFormat, Luma};
    let img: ImageBuffer<Luma<u8>, Vec<u8>> = ImageBuffer::from_pixel(1, 1, Luma([255u8]));
    let mut buf = std::io::Cursor::new(Vec::new());
    let _ = img.write_to(&mut buf, ImageFormat::Png);
    buf.into_inner()
}
