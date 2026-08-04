use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::SqlitePool;

use core_api::user_fs::SharedFs;

use crate::chat_hub::ChatHub;
use crate::db::memory_docs;
use crate::events::{GlobalEvent, ServerEvent};
use crate::session::handler::{InterfaceTool, ToolFuture};
use crate::tools::fs;
use crate::tools::tool_names::SHOW_FILE_TO_USER;

/// Build a `show_file_to_user` InterfaceTool bound to a `ChatHub`, a source, the
/// caller's [`SharedFs`] (their per-user filesystem view) and the two memory
/// pools (the caller's own + the shared system one).
///
/// Injected only for SPA clients (web copilot + mobile) at the WebSocket entry
/// point, so Telegram — which has its own `send_attachment` — never sees it.
///
/// The path is resolved through the caller's own workspace (`resolve_view_path`):
/// `~/…`, `shared/{X}/…`, `projects/{O}/{S}/…`, a bare relative path, or a
/// container-absolute `/root/…` — anything outside the container view is refused.
/// A path under a memory root (`user-memory/…`, `shared-memory/…`) is a virtual
/// note instead: it is looked up in `memory_docs` on the matching pool — the
/// viewer's `GET /api/file` applies the same routing, so it round-trips.
/// It then emits a `ServerEvent::OpenFile` carrying the **canonical agent path**, so
/// the file-viewer page fetches the same file back through `/api/file`. The
/// frontend renders every kind in the viewer (HTML live in an origin-isolated
/// iframe; LaTeX compiled to PDF server-side).
///
/// `session_id` is the conversation this instance belongs to: clients filter
/// events per conversation, so an untagged `OpenFile` would reach nobody.
pub fn make_tool(
    hub: Arc<ChatHub>,
    source: String,
    session_id: i64,
    fs: SharedFs,
    user_pool: SqlitePool,
    shared_pool: SqlitePool,
) -> InterfaceTool {
    let definition = json!({
        "type": "function",
        "function": {
            "name": SHOW_FILE_TO_USER,
            "description": "Show a file to the user by opening it in their interface. \
                             Supports Markdown, source code, plain text, raster images \
                             (PNG/JPG/GIF/WebP/…), SVG, PDF, and LaTeX (.tex — compiled \
                             to PDF automatically on the server). HTML files open in a \
                             new browser tab. Use this to surface a file you created or \
                             found so the user can look at it directly. One file per call. \
                             The file must already exist on disk — or as a memory note \
                             (`user-memory/…`, `shared-memory/…`). \
                             IMPORTANT for LaTeX: always pass the `.tex` source, never a \
                             pre-built `.pdf` of a document you have the `.tex` for. The \
                             `.tex` is compiled on the server and the view live-reloads \
                             whenever any of its dependencies (\\input fragments, .sty/.cls, \
                             images) change. A raw `.pdf` is served statically — never \
                             recompiled and its dependencies are not watched — so the user \
                             would keep seeing a stale render.",
            "parameters": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path of the file to show, in your own workspace: relative to your \
                                         home (e.g. `report.md` or `~/report.md`), a `shared/<folder>/…` or \
                                         `projects/<owner>/<slug>/…` path, a memory note \
                                         (`user-memory/…`, `shared-memory/…`), or an absolute container \
                                         path (`/root/…`). Paths outside your workspace are refused."
                    }
                },
                "required": ["path"]
            }
        }
    });

    let handler = Arc::new(move |args: Value| -> ToolFuture {
        let hub         = Arc::clone(&hub);
        let source      = source.clone();
        let fs          = fs.clone();
        let user_pool   = user_pool.clone();
        let shared_pool = shared_pool.clone();
        Box::pin(async move {
            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("show_file_to_user: missing required parameter 'path'"))?;

            // Virtual memory namespace → a `memory_docs` note, not a disk file.
            // The viewer serves it through the same routing (see GET /api/file),
            // so confirming existence is all that's needed here.
            if let Some(mem) = fs::classify_memory(path) {
                if mem.rel.is_empty() {
                    anyhow::bail!("show_file_to_user: '{path}' is a memory folder, not a file");
                }
                let (pool, root) = match mem.scope {
                    fs::MemScope::User   => (&user_pool, fs::USER_MEMORY_ROOT),
                    fs::MemScope::Shared => (&shared_pool, fs::SHARED_MEMORY_ROOT),
                };
                let exists = memory_docs::get(pool, &mem.rel).await
                    .map_err(|e| anyhow::anyhow!("show_file_to_user: {e}"))?
                    .is_some();
                if !exists {
                    anyhow::bail!("show_file_to_user: file not found: {path}");
                }
                let display = format!("{root}/{}", mem.rel);
                hub.emit(GlobalEvent {
                    source:     Some(source),
                    session_id: Some(session_id),
                    event:      ServerEvent::OpenFile { path: display.clone() },
                });
                return Ok(format!("Opened {display} in the user's viewer."));
            }

            // Resolve against the caller's workspace snapshot: gives the host path to
            // stat and the canonical agent path the viewer will fetch back.
            let user_fs = fs.load();
            let (target, display) = fs::resolve_view_target(user_fs.as_ref(), path)
                .map_err(|e| anyhow::anyhow!("show_file_to_user: {e}"))?;
            // A container-only path is statted through the container, the same way
            // the viewer will fetch it back.
            let (exists, is_dir) = match &target {
                fs::FsTarget::Host(abs) => (abs.exists(), abs.is_dir()),
                fs::FsTarget::Container { container, path } => (
                    crate::container::exec_fs::exists(container, path).await,
                    crate::container::exec_fs::is_dir(container, path).await,
                ),
            };
            if !exists {
                anyhow::bail!("show_file_to_user: file not found: {display}");
            }
            if is_dir {
                anyhow::bail!("show_file_to_user: '{display}' is a directory, not a file");
            }

            hub.emit(GlobalEvent {
                source:     Some(source),
                session_id: Some(session_id),
                event:      ServerEvent::OpenFile { path: display.clone() },
            });
            Ok(format!("Opened {display} in the user's viewer."))
        })
    });

    InterfaceTool { definition, handler }
}
