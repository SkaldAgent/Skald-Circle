//! Honcho's HTTP surface, mounted by the main `WebFrontend` under
//! `/api/plugin/honcho/` behind Skald's normal auth + enabled-gate.
//!
//! Deliberately small. It serves the two page fragments (the admin config page
//! and the user opt-in page) and one admin action, `POST /admin/test`, a
//! connectivity check against a candidate config. The opt-in toggle and the
//! config save reuse the **core** plugin endpoints (`PUT /api/plugins/honcho`
//! and `/api/plugins/honcho/my-config`), so nothing about persistence lives
//! here.
//!
//! Honcho does **not** `manages_own_access`, so — unlike mobile-connector — the
//! `plugin_access` grant is *not* an admin check (it is `true` for every granted
//! user). The admin endpoint therefore gates on the real
//! [`UserChannelApi::is_admin`].
//!
//! Every request resolves the *current* wiring through the shared [`WebCell`]
//! (filled on `start`, cleared on `stop`), so a reconfigure is transparent and a
//! request that arrives while the plugin is enabled-but-not-running gets a clean
//! 503 rather than a stale snapshot.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use core_api::i18n::I18nApi;
use core_api::plugin::Caller;
use core_api::user_channel::UserChannelApi;
use honcho_client::HonchoClient;
use honcho_client::models::{PageParams, WorkspaceGet};

// Namespaced i18n keys for the router's user-facing strings (backend tables in
// `../i18n/*.json`), resolved to the caller's language via `web.i18n`.
const KEY_ADMIN_ONLY:     &str = "plugin.honcho.err.admin_only";
const KEY_BASE_URL_EMPTY: &str = "plugin.honcho.err.base_url_empty";
const KEY_TEST_FAILED:    &str = "plugin.honcho.err.test_failed";

/// Deps the router needs at request time.
#[derive(Clone)]
pub struct HonchoWeb {
    pub user_channel: Arc<dyn UserChannelApi>,
    pub i18n:         Arc<dyn I18nApi>,
}

/// Shared cell: an `Arc` to a `Mutex` holding the (optional) live wiring. Cloned
/// cheaply and shared between the plugin (`start`/`stop`) and the router.
pub type WebCell = Arc<tokio::sync::Mutex<Option<HonchoWeb>>>;

/// Build the plugin's router. Takes the shared cell so each request resolves the
/// *current* wiring — not a snapshot from startup.
pub fn build(cell: WebCell) -> Router {
    Router::new()
        // Page fragments (served as ES modules to the browser).
        .route("/web/config.js", get(|| async { serve_js(include_str!("../web/config.js")) }))
        .route("/web/memory.js", get(|| async { serve_js(include_str!("../web/memory.js")) }))
        .route("/web/common.js", get(|| async { serve_js(include_str!("../web/common.js")) }))
        .route("/web/i18n.js",   get(|| async { serve_js(include_str!("../web/i18n.js")) }))
        // Admin: validate a candidate connection before saving it.
        .route("/admin/test", post(admin_test))
        // Predisposition for the user page's future "what does Honcho know about
        // me?" panel: a `GET /whoami` here would resolve the `Caller`'s user id,
        // gate on `opted_in`, and call the live `HonchoMemory` client's
        // `peer_chat` (Dialectic) / `peer_context` for that user's peer. Not
        // shipped in v1 — the opt-in page needs no backend of its own.
        .with_state(cell)
}

fn serve_js(body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], body).into_response()
}

/// Resolve the live wiring, or `503` while the plugin is enabled but not running.
async fn web_or_503(cell: &WebCell) -> Result<HonchoWeb, Response> {
    cell.lock().await.clone().ok_or_else(|| {
        (StatusCode::SERVICE_UNAVAILABLE, "honcho is not running").into_response()
    })
}

/// Fail-closed admin gate for the built-in admin role.
async fn require_admin(web: &HonchoWeb, caller: &Caller) -> Result<(), Response> {
    if web.user_channel.is_admin(&caller.user_id).await {
        Ok(())
    } else {
        let msg = web.i18n.for_user(&caller.user_id, KEY_ADMIN_ONLY, &[]).await;
        Err((StatusCode::FORBIDDEN, msg).into_response())
    }
}

// ── POST /admin/test ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TestBody {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key:  String,
}

/// Admin connectivity check against a *candidate* config (the unsaved draft), so
/// an admin can validate a URL/key before saving. Builds a throwaway client and
/// lists workspaces — verifies the URL is reachable and the key is accepted
/// without creating or mutating anything on the server.
async fn admin_test(
    State(cell):       State<WebCell>,
    Extension(caller): Extension<Caller>,
    Json(body):        Json<TestBody>,
) -> Response {
    let web = match web_or_503(&cell).await {
        Ok(w) => w,
        Err(r) => return r,
    };
    if let Err(r) = require_admin(&web, &caller).await {
        return r;
    }

    let base_url = body.base_url.trim();
    if base_url.is_empty() {
        let msg = web.i18n.for_user(&caller.user_id, KEY_BASE_URL_EMPTY, &[]).await;
        return (StatusCode::BAD_REQUEST, msg).into_response();
    }

    let client = HonchoClient::with_base_url(base_url, body.api_key.trim());
    match client
        .list_workspaces(&PageParams::default(), &WorkspaceGet::default())
        .await
    {
        Ok(page) => Json(json!({ "ok": true, "workspaces": page.total })).into_response(),
        Err(e) => {
            let msg = web
                .i18n
                .for_user(&caller.user_id, KEY_TEST_FAILED, &[("detail", &e.to_string())])
                .await;
            (StatusCode::BAD_GATEWAY, msg).into_response()
        }
    }
}
