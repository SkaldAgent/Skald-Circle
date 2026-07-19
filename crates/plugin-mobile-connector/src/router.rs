//! The plugin's HTTP surface, mounted by the main `WebFrontend` under
//! `/api/plugin/mobile-connector/` behind Skald's normal auth + enabled-gate.
//!
//! Two audiences on one router:
//! - the **QR endpoint** (`/pairingqrcode`) — renders the pairing QR PNG on
//!   demand from the in-memory session (no QR ever touches disk);
//! - the **admin pairing console** — the JSON API + the two page fragments
//!   (`web/pairing.js`, `web/devices.js`) that let an admin pair, list, bind and
//!   revoke devices from the browser instead of driving the LLM control tools.
//!
//! Every request resolves the *current* [`RelayApp`] through the shared state
//! cell (`Arc<Mutex<Option<Arc<RelayApp>>>>`), so a reconfigure (reload → fresh
//! `RelayApp`) is transparent. Management endpoints are admin-only: the router
//! runs inside `require_auth` (which injects [`Caller`]) and gates on
//! [`UserChannelApi::plugin_access`], which — because the connector
//! `manages_own_access` — returns `true` only for admins.

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
// `../i18n/*.json`). Resolved to the caller's language via `app.i18n()`. Every
// use sits after `admin_app`, so the app — hence the localizer — is present.
const KEY_RELAY_NOT_CONNECTED: &str = "plugin.mobile-connector.err.relay_not_connected";
const KEY_ADMIN_ONLY:          &str = "plugin.mobile-connector.err.admin_only";
const KEY_USER_ID_EMPTY:       &str = "plugin.mobile-connector.err.user_id_empty";
const KEY_PUBKEY_HEX:          &str = "plugin.mobile-connector.err.pubkey_hex";

/// Build the plugin's router. Takes the shared state cell so each request
/// resolves the *current* `RelayApp` — not a snapshot from startup.
pub fn build(state_cell: StateCell) -> Router {
    Router::new()
        .route("/pairingqrcode", get(pairing_qr))
        // Page fragments (served as ES modules to the browser).
        .route("/web/pairing.js", get(|| async { serve_js(include_str!("../web/pairing.js")) }))
        .route("/web/devices.js", get(|| async { serve_js(include_str!("../web/devices.js")) }))
        .route("/web/common.js", get(|| async { serve_js(include_str!("../web/common.js")) }))
        .route("/web/i18n.js",   get(|| async { serve_js(include_str!("../web/i18n.js")) }))
        // Admin pairing console API.
        .route("/pairing", post(start_pairing).delete(stop_pairing))
        .route("/devices", get(list_devices))
        .route("/devices/bind", post(bind_device))
        .route("/devices/revoke", post(revoke_device))
        .with_state(state_cell)
}

// ── Admin console: shared plumbing ──────────────────────────────────────────────

/// Resolve the live app, or `503` when the plugin is enabled but its runloop is
/// not up (e.g. no `relay_url` configured).
async fn app_or_503(cell: &StateCell) -> Result<Arc<RelayApp>, Response> {
    cell.lock().await.as_ref().map(Arc::clone).ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "mobile connector is not running").into_response()
    })
}

/// Fail-closed admin gate. For a `manages_own_access` connector nobody holds a
/// `plugin_access` grant, so this is `true` only for the built-in admin role.
async fn require_admin(app: &RelayApp, caller: &Caller) -> Result<(), Response> {
    if app.user_channel.plugin_access(PLUGIN_ID, &caller.user_id).await {
        Ok(())
    } else {
        let msg = app.i18n().for_user(&caller.user_id, KEY_ADMIN_ONLY, &[]).await;
        Err((StatusCode::FORBIDDEN, msg).into_response())
    }
}

/// Resolve the app and check admin in one step (the common prelude).
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

// ── POST/DELETE /pairing ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct StartPairingBody {
    /// Window lifetime in seconds; `0`/absent = the configured default, capped at 600.
    #[serde(default)]
    ttl: Option<u32>,
}

/// Open a pairing window and return the QR URL. The caller (an admin) becomes
/// the pending owner, so a device that pairs in this window auto-binds to them.
async fn start_pairing(
    State(cell): State<StateCell>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<StartPairingBody>,
) -> Response {
    let app = match admin_app(&cell, &caller).await { Ok(a) => a, Err(r) => return r };
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

/// Close the pairing window and disarm auto-binding.
async fn stop_pairing(
    State(cell): State<StateCell>,
    Extension(caller): Extension<Caller>,
) -> Response {
    let app = match admin_app(&cell, &caller).await { Ok(a) => a, Err(r) => return r };
    app.set_pending_owner(None).await;
    match app.client().stop_pairing().await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ── GET /devices ────────────────────────────────────────────────────────────────

/// List every known device, each tagged with its bound user, state and metadata.
async fn list_devices(
    State(cell): State<StateCell>,
    Extension(caller): Extension<Caller>,
) -> Response {
    let app = match admin_app(&cell, &caller).await { Ok(a) => a, Err(r) => return r };
    let rows = app.client().list_clients().await;
    let bindings = app.bindings.read().await;
    let devices: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let pk_hex = hex::encode(r.ed25519_pub);
            let bound_user = bindings.user_for_pubkey(&pk_hex);
            let device_info: Option<Value> =
                r.device_info.as_deref().and_then(|s| serde_json::from_str(s).ok());
            json!({
                "pubkey":      pk_hex,
                "state":       if r.state == ClientState::Authorized { "authorized" } else { "pending" },
                "bound_user":  bound_user,
                "platform":    r.platform,
                "device_info": device_info,
                "last_seen":   r.last_seen,
            })
        })
        .collect();
    Json(json!({ "devices": devices })).into_response()
}

// ── POST /devices/bind + /devices/revoke ────────────────────────────────────────

#[derive(Deserialize)]
struct BindBody {
    pubkey: String,
    user_id: String,
    #[serde(default)]
    display: Option<String>,
}

/// Bind (or reassign) a device to a user and authorize it.
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

/// Revoke a device and drop its binding.
async fn revoke_device(
    State(cell): State<StateCell>,
    Extension(caller): Extension<Caller>,
    Json(body): Json<RevokeBody>,
) -> Response {
    let app = match admin_app(&cell, &caller).await { Ok(a) => a, Err(r) => return r };
    let pk = match decode_pubkey(&app, &caller, &body.pubkey).await { Ok(p) => p, Err(r) => return r };
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
