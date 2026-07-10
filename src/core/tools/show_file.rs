use std::sync::Arc;

use serde_json::{Value, json};

use crate::core::chat_hub::ChatHub;
use crate::core::events::{GlobalEvent, ServerEvent};
use crate::core::session::handler::{InterfaceTool, ToolFuture};
use crate::core::tools::fs;
use crate::core::tools::tool_names::SHOW_FILE_TO_USER;

/// Build a `show_file_to_user` InterfaceTool bound to a `ChatHub` and a source.
///
/// Injected only for SPA clients (web copilot + mobile) at the WebSocket entry
/// point, so Telegram — which has its own `send_attachment` — never sees it.
///
/// When called, it emits a `ServerEvent::OpenFile` to the source's connected
/// clients. The frontend routes it: HTML opens in a new browser tab, everything
/// else (Markdown / code / raster images / SVG / PDF / LaTeX — which is compiled
/// to PDF server-side) opens in the file-viewer page.
pub fn make_tool(hub: Arc<ChatHub>, source: String) -> InterfaceTool {
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
                             The file must already exist on disk. \
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
                        "description": "Path of the file to show. Relative to the project root, or absolute."
                    }
                },
                "required": ["path"]
            }
        }
    });

    let handler = Arc::new(move |args: Value| -> ToolFuture {
        let hub    = Arc::clone(&hub);
        let source = source.clone();
        Box::pin(async move {
            let path = args["path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("show_file_to_user: missing required parameter 'path'"))?;

            let abs = fs::resolve(path)?;
            if !abs.exists() {
                anyhow::bail!("show_file_to_user: file not found: {path}");
            }
            if abs.is_dir() {
                anyhow::bail!("show_file_to_user: '{path}' is a directory, not a file");
            }

            let display = fs::relativize_for_display(path);
            hub.emit(GlobalEvent {
                source:     Some(source),
                session_id: None,
                event:      ServerEvent::OpenFile { path: display.clone() },
            });
            Ok(format!("Opened {display} in the user's viewer."))
        })
    });

    InterfaceTool { definition, handler }
}
