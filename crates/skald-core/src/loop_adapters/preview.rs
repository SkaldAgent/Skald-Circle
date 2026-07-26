//! File-write diff preview, shared by the approval gate (pre-approval diff)
//! and the write-preview hook (executed-write diff). Routes memory-vs-disk
//! exactly like the fs-tools.

use std::sync::Arc;

use core_api::user_fs::SharedFs;
use sqlx::SqlitePool;

use crate::tools::fs::{MemScope, classify_memory, resolve_host_path};

/// Max bytes captured per side of a file-write diff preview. Beyond this the
/// side is dropped (`None`) so a huge file never bloats a row or a WS payload.
pub const MAX_PREVIEW_BYTES: usize = 256 * 1024;

/// Drops a captured snapshot over the size cap (a truncated snapshot would
/// render a misleading diff).
pub fn cap_preview(s: Option<String>) -> Option<String> {
    s.filter(|c| c.len() <= MAX_PREVIEW_BYTES)
}

/// The pieces a preview read needs: owner pool (user-memory), shared pool
/// (shared-memory), and the caller's fs view (host paths).
#[derive(Clone)]
pub struct PreviewContext {
    pub pool:        Arc<SqlitePool>,
    pub shared_pool: Arc<SqlitePool>,
    pub fs:          Option<SharedFs>,
}

/// Reads the current content of a file for a diff, routed exactly like the
/// fs-tools. A resolve failure or a missing note/file yields `None`
/// (rendered as "new file").
pub async fn read_current_content(ctx: &PreviewContext, path: &str) -> Option<String> {
    if let Some(m) = classify_memory(path) {
        let pool = match m.scope {
            MemScope::User   => &ctx.pool,
            MemScope::Shared => &ctx.shared_pool,
        };
        return crate::db::memory_docs::get(pool, &m.rel)
            .await.ok().flatten().map(|d| d.content);
    }
    let fs = ctx.fs.as_ref()?;
    let abs = resolve_host_path(&fs.load(), path).ok()?;
    tokio::fs::read_to_string(&abs).await.ok()
}

/// Computes what a file would look like after the tool runs, without writing
/// it. `None` if indeterminable (e.g. edit on a missing file).
pub async fn compute_new_content(ctx: &PreviewContext, name: &str, args: &serde_json::Value) -> Option<String> {
    match name {
        "write_file" => args["content"].as_str().map(|s| s.to_string()),
        "edit_file" => {
            let path     = args["path"].as_str()?;
            let old_text = args["old"].as_str()?;
            let new_text = args["new"].as_str()?;
            let current  = read_current_content(ctx, path).await?;
            if current.contains(old_text) {
                Some(current.replacen(old_text, new_text, 1))
            } else {
                None
            }
        }
        "insert_at_line" => {
            let path      = args["path"].as_str()?;
            let line_num  = args["line"].as_u64()? as usize;
            let new_text  = args["content"].as_str()?;
            let placement = args["placement"].as_str().unwrap_or("after");
            if line_num == 0 { return None; }
            let current = read_current_content(ctx, path).await?;
            let mut lines: Vec<&str> = current.split('\n').collect();
            let idx        = (line_num - 1).min(lines.len().saturating_sub(1));
            let insert_idx = if placement == "before" { idx } else { idx + 1 };
            let new_lines: Vec<&str> = new_text.split('\n').collect();
            for (i, l) in new_lines.iter().enumerate() {
                lines.insert(insert_idx + i, l);
            }
            Some(lines.join("\n"))
        }
        "replace_lines" => {
            let path      = args["path"].as_str()?;
            let from_line = args["from_line"].as_u64()? as usize;
            let to_line   = args["to_line"].as_u64()? as usize;
            let new_text  = args["new"].as_str()?;
            if from_line == 0 || to_line < from_line { return None; }
            let current = read_current_content(ctx, path).await?;
            let mut lines: Vec<&str> = current.lines().collect();
            let total = lines.len();
            if from_line > total { return None; }
            let to_clamped = to_line.min(total);
            let new_lines: Vec<&str> = new_text.lines().collect();
            lines.splice((from_line - 1)..to_clamped, new_lines);
            let has_trailing = current.ends_with('\n');
            let mut result = lines.join("\n");
            if has_trailing { result.push('\n'); }
            Some(result)
        }
        _ => None,
    }
}
