//! The single HTTP route the plugin contributes: the runtime QR-code endpoint
//! (plugin.md §5). Mounted by the main `WebFrontend` under
//! `/api/plugin/mobile-connector/` behind Skald's normal auth. No QR is ever
//! written to disk — the PNG is rendered on demand from the in-memory session.
//!
//! The router receives the plugin's shared state cell
//! (`Arc<Mutex<Option<Arc<RelayState>>>>`) so that every request resolves the
//! **current** `RelayState` — the same one the LLM tools use.  This avoids the
//! classic stale-Arc bug when the plugin is reconfigured (reload stops the old
//! runloop + creates a fresh `RelayState`, but the router is only built once).

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use tokio::sync::Mutex;

use skald_relay_client::SessionState;

use crate::app::RelayApp;

/// Shared cell type: an `Arc` to a `Mutex` holding the (optional) live app.
/// Cloned cheaply and safely shared between the plugin and the router.
type StateCell = Arc<Mutex<Option<Arc<RelayApp>>>>;

#[derive(Deserialize)]
struct QrQuery {
    code: Option<String>,
}

/// Build the plugin's router. Takes the shared state cell so each request
/// resolves the *current* `RelayState` — not a snapshot from startup.
pub fn build(state_cell: StateCell) -> Router {
    Router::new()
        .route("/pairingqrcode", get(pairing_qr))
        .with_state(state_cell)
}

/// `GET /pairingqrcode?code=<random>` → PNG of the QR while active, else a
/// placeholder PNG (plugin.md §5 table).
async fn pairing_qr(
    State(cell): State<StateCell>,
    Query(q): Query<QrQuery>,
) -> impl IntoResponse {
    let Some(code) = q.code else {
        return png_response(render_placeholder("QR non valido"));
    };

    // Dynamically resolve the *current* RelayApp (same one tools use).
    let app = match cell.lock().await.as_ref() {
        Some(s) => Arc::clone(s),
        None => return png_response(render_placeholder("Plugin non attivo")),
    };

    match app.client().lookup_pairing(&code) {
        Some((qr, SessionState::Active)) => {
            // Encode the normative QrCodeData JSON into the QR.
            match serde_json::to_string(&qr) {
                Ok(json) => match render_qr(&json) {
                    Ok(png) => png_response(png),
                    Err(_) => png_response(render_placeholder("Errore QR")),
                },
                Err(_) => png_response(render_placeholder("Errore QR")),
            }
        }
        Some((_, SessionState::Consumed)) => png_response(render_placeholder("QR già usato")),
        Some((_, SessionState::Superseded)) => png_response(render_placeholder("QR scaduto")),
        None => png_response(render_placeholder("QR scaduto")),
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
