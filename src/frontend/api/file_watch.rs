//! File-watch WebSocket endpoint.
//!
//! `GET /api/file/watch` upgrades to a long-lived WebSocket. The client sends
//! JSON commands:
//!
//! ```jsonc
//! { "op": "subscribe",   "path": "docs/index.md" }   // start watching
//! { "op": "unsubscribe", "path": "docs/index.md" }   // stop watching
//! ```
//!
//! The server pushes change notifications:
//!
//! ```jsonc
//! { "type": "subscribed",   "path": "..." }   // ack after a successful subscribe
//! { "type": "unsubscribed", "path": "..." }   // ack after an unsubscribe
//! { "type": "changed",      "path": "..." }   // file changed on disk
//! { "type": "error",        "path": "...", "error": "..." }   // watch install failed
//! ```
//!
//! `path` is the original client-supplied string (an agent path — `~/…`,
//! `shared/{X}/…`, `projects/{O}/{S}/…` — or a container-absolute `/root/…`) — it
//! round-trips unchanged so the client can match it against the path it asked to
//! watch. The backend resolves it to an absolute host path via
//! `fs_tools::resolve_view_path` scoped to the connection's authenticated user
//! (same path model as `GET /api/file`); a path outside that user's workspace is
//! refused fail-closed.
//!
//! One OS watcher per watched file per connection (no cross-connection
//! sharing). On disconnect every watcher is dropped and the OS resources are
//! released automatically.
//!
//! ## What counts as a change
//!
//! Only events that move the file's content version (mtime + len) are
//! forwarded; pure reads (`Access`) and metadata-only touches are dropped in
//! the watcher callback. Without that filter the viewer's own `GET /api/file`
//! read would re-trigger the watcher on every reload, looping forever.
//!
//! ## LaTeX dependency-aware watching
//!
//! When subscribing to a `.tex` / `.latex` source, the server expands the
//! single path into the full dependency set discovered via the `LatexCompiler`'s
//! `.fls` sidecar (every `\input`'ed fragment, custom `.sty` / `.cls`,
//! `.bib`, images, etc.). One OS watcher is installed per dependency. Any
//! change to any of them is forwarded to the client as a `changed` event for
//! the original `.tex` path — so the file viewer does not need to know about
//! the dependency graph.
//!
//! The dependency set is re-synced whenever the main `.tex` changes (or any of
//! its dependencies does): the watchers for that path are dropped and
//! re-installed with the fresh `.fls` content, so newly-added `\input`s are
//! picked up automatically. On the very first subscribe, when no compile has
//! happened yet, only the main `.tex` itself is watched; once the viewer's
//! first compile writes the `.fls`, the next change event triggers the re-sync.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Extension,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use core_api::user_fs::SharedFs;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::info;

use skald_core::skald::Skald;
use skald_core::tools::fs as fs_tools;

use super::guard::AuthUser;

pub async fn handler(
    ws:              WebSocketUpgrade,
    Extension(auth): Extension<AuthUser>,
    State(skald):    State<Arc<Skald>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, skald, auth.user_id))
}

#[derive(Deserialize)]
struct ClientMsg {
    op: String,
    path: String,
}

async fn handle_socket(mut socket: WebSocket, skald: Arc<Skald>, user_id: String) {
    // Resolve the caller's per-user filesystem view once at connect; every
    // subscribe resolves its path against the current snapshot (`fs.load()`), so a
    // membership change is picked up without dropping the connection. A missing
    // context means the database re-locked — report and close.
    let fs: SharedFs = match skald.user_context(&user_id).await {
        Some(ctx) => ctx.fs.clone(),
        None => {
            let _ = send_json(&mut socket,
                json!({ "type": "error", "error": "session expired — please log in again" })
            ).await;
            return;
        }
    };
    info!(user = %user_id, "file-watch WS connected");

    // Single mpsc into which every watcher callback forwards via an unbounded
    // sender (unbounded so the sync callback never blocks).
    let (change_tx, mut change_rx) = mpsc::unbounded_channel::<String>();

    // original_path -> watchers (dropping the vec un-watches every path).
    // A vec per subscription because LaTeX sources expand to one watcher per
    // dependency.
    let mut watchers: HashMap<String, Vec<RecommendedWatcher>> = HashMap::new();

    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let parsed = match serde_json::from_str::<ClientMsg>(&text) {
                            Ok(p) => p,
                            Err(e) => {
                                let _ = send_json(&mut socket,
                                    json!({ "type": "error", "error": format!("bad message: {e}") })
                                ).await;
                                continue;
                            }
                        };
                        match parsed.op.as_str() {
                            "subscribe" => {
                                if watchers.contains_key(&parsed.path) {
                                    // Already subscribed — silently ack.
                                    let _ = send_json(&mut socket,
                                        json!({ "type": "subscribed", "path": parsed.path })
                                    ).await;
                                    continue;
                                }
                                match install_watcher(&parsed.path, &change_tx, &mut watchers, &skald, &fs) {
                                    Ok(()) => {
                                        let _ = send_json(&mut socket,
                                            json!({ "type": "subscribed", "path": parsed.path })
                                        ).await;
                                    }
                                    Err(err) => {
                                        let _ = send_json(&mut socket,
                                            json!({ "type": "error", "path": parsed.path, "error": err })
                                        ).await;
                                    }
                                }
                            }
                            "unsubscribe" => {
                                if watchers.remove(&parsed.path).is_some() {
                                    let _ = send_json(&mut socket,
                                        json!({ "type": "unsubscribed", "path": parsed.path })
                                    ).await;
                                }
                            }
                            other => {
                                let _ = send_json(&mut socket,
                                    json!({ "type": "error", "error": format!("unknown op: {other}") })
                                ).await;
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }

            changed_path = change_rx.recv() => {
                if let Some(p) = changed_path {
                    if send_json(&mut socket, json!({ "type": "changed", "path": p })).await.is_err() {
                        break;
                    }
                    // For LaTeX sources, the dependency set may have changed
                    // (e.g. a new \input was added, or the first compile just
                    // wrote the .fls). Drop & re-install the watchers for that
                    // path so they reflect the current dependency graph.
                    if is_latex_path(&p) {
                        if watchers.remove(&p).is_some() {
                            let _ = install_watcher(&p, &change_tx, &mut watchers, &skald, &fs);
                        }
                    }
                }
            }
        }
    }

    info!("file-watch WS disconnected");
}

/// Create one `RecommendedWatcher` per watched path and store them in
/// `watchers` keyed by the original (un-resolved) `user_path`. Returns an
/// error string on failure so the caller can report it to the client.
///
/// For `.tex` / `.latex` sources the single user path is expanded into the
/// full dependency set via `LatexCompiler::watch_paths_for` (every
/// `\input`'ed file, custom `.sty`/`.cls`, `.bib`, images, etc.). All events
/// for any dependency are forwarded to the client as a `changed` event for
/// the original `.tex` path.
fn install_watcher(
    user_path: &str,
    change_tx: &mpsc::UnboundedSender<String>,
    watchers: &mut HashMap<String, Vec<RecommendedWatcher>>,
    skald: &Skald,
    fs: &SharedFs,
) -> Result<(), String> {
    let (abs, _agent): (PathBuf, String) =
        fs_tools::resolve_view_path(fs.load().as_ref(), user_path).map_err(|e| e.to_string())?;

    let paths_to_watch: Vec<PathBuf> = if is_latex_path(user_path) {
        skald.latex_compiler().watch_paths_for(&abs)
    } else {
        vec![abs]
    };

    let mut installed: Vec<RecommendedWatcher> = Vec::with_capacity(paths_to_watch.len());

    for path in paths_to_watch {
        let tx_for_cb = change_tx.clone();
        let original_path = user_path.to_string();
        let path_for_cb = path.clone();
        // Content-version baseline for the stat dedup in the callback.
        let mut last_stamp = content_stamp(&path);

        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| {
                let Ok(event) = res else { return };
                // A pure read is never a change — and it matters doubly here,
                // because the viewer's own `GET /api/file` produces exactly
                // these events (IN_ACCESS / IN_CLOSE_NOWRITE on Linux):
                // forwarding them made every client reload re-trigger the
                // watcher, a self-sustaining reload loop. Real writes still
                // surface as Modify events, so dropping Access loses nothing.
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                // Every other kind is answered with the one question that
                // matters — did (mtime, len) actually move? This swallows
                // metadata-only noise (atime, chmod) whatever kind the backend
                // reports it as, without trusting per-platform kind mappings.
                // A failed stat means the file is gone, and that IS a change.
                let stamp = content_stamp(&path_for_cb);
                if stamp == last_stamp {
                    return;
                }
                last_stamp = stamp;
                if tx_for_cb.send(original_path.clone()).is_err() {
                    // channel closed — receiver dropped (WS disconnected).
                }
            },
            Config::default(),
        )
        .map_err(|e| format!("watcher create failed: {e}"))?;

        // Skip non-existent paths gracefully: a dependency may have been
        // removed since the .fls was last written. The next compile will
        // refresh the .fls and the watcher set will be re-synced.
        if path.exists() {
            watcher
                .watch(&path, RecursiveMode::NonRecursive)
                .map_err(|e| format!("watch install failed for {}: {e}", path.display()))?;
        }

        installed.push(watcher);
    }

    watchers.insert(user_path.to_string(), installed);
    Ok(())
}

/// Content version of a file as (mtime_ns, len) — the same inputs as the HTTP
/// `ETag` (`disk_etag` in `api/files.rs`). A pure read never moves it, any
/// write does. `None` when the file is missing or unreadable.
fn content_stamp(path: &Path) -> Option<(u128, u64)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime_ns = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((mtime_ns, md.len()))
}

/// True for `.tex` / `.latex` extensions — sources that trigger the
/// dependency-aware watcher expansion.
fn is_latex_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "tex" | "latex"))
        .unwrap_or(false)
}

async fn send_json(socket: &mut WebSocket, value: serde_json::Value) -> Result<(), axum::Error> {
    socket.send(Message::Text(value.to_string().into())).await
}
