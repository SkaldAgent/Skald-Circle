use serde_json::Value;
use tracing::debug;

use super::ChatSessionHandler;
use super::emitter::TurnEmitter;
use crate::tools::{is_file_write_tool, tool_names as tn};

impl ChatSessionHandler {
    /// Emits the appropriate frontend approval event for the given tool call.
    ///
    /// | Tool kind        | Event emitted                                         |
    /// |------------------|-------------------------------------------------------|
    /// | file-write tools | `PendingWrite` with before/after diff (IO concurrent) |
    /// | `execute_cmd`    | `PendingWrite` with command preview                   |
    /// | `restart`        | `PendingWrite` with restart description               |
    /// | everything else  | `ApprovalRequired`                                    |
    ///
    /// Called from both `llm_loop` and `resume_pending_tools` to avoid duplication.
    pub(super) async fn emit_approval_event(
        &self,
        em:           &TurnEmitter<'_>,
        request_id:   i64,
        tool_call_id: i64,
        tool_name:    &str,
        arguments:    &Value,
    ) {
        if is_file_write_tool(tool_name) {
            let path = arguments["path"].as_str().unwrap_or("").to_string();
            // Read current file and compute new content concurrently — both are disk I/O.
            let (old_content, new_content) = tokio::join!(
                self.read_current_content(&path),
                self.compute_new_content(tool_name, arguments),
            );
            if let Some(new_content) = new_content {
                em.pending_write(request_id, tool_call_id, path, old_content, new_content).await;
            } else {
                // File doesn't exist yet or diff can't be computed — fall back to generic.
                debug!(tool = tool_name, "emit_approval_event: no diff available, using ApprovalRequired");
                em.approval_required(request_id, tool_call_id, tool_name.to_string(), arguments.clone()).await;
            }
        } else if tool_name == tn::EXECUTE_CMD {
            let cmd = arguments["command"].as_str().unwrap_or("");
            em.pending_write(request_id, tool_call_id, "$ execute_cmd".to_string(), None, format!("$ {cmd}")).await;
        } else {
            em.approval_required(request_id, tool_call_id, tool_name.to_string(), arguments.clone()).await;
        }
    }

    /// Reads the current content of a file for the diff in a `PendingWrite` event.
    ///
    /// Routes **exactly like the fs-tools** (blueprint §6), so the diff the user
    /// approves reflects the real target — not the server's cwd:
    /// - `user-memory/…` / `shared-memory/…` → the `memory_docs` note on the right
    ///   pool (owner vs `system.db`), never disk;
    /// - every other agent path → the caller's per-user host workspace via `self.fs`,
    ///   containment-checked by `resolve_host_path`.
    ///
    /// A resolve failure or a missing note/file yields `None` (rendered as "new file").
    /// The old cwd-relative `fs::resolve` was wrong for every agent path: it showed a
    /// bogus "new file" on overwrites and, worse, the diff of a same-named cwd file.
    pub(super) async fn read_current_content(&self, path: &str) -> Option<String> {
        use crate::tools::fs::{classify_memory, resolve_host_path, MemScope};
        if let Some(m) = classify_memory(path) {
            let pool = match m.scope {
                MemScope::User   => &self.db,
                MemScope::Shared => &self.shared_pool,
            };
            return crate::db::memory_docs::get(pool, &m.rel)
                .await.ok().flatten().map(|d| d.content);
        }
        let abs = resolve_host_path(&self.fs.load(), path).ok()?;
        tokio::fs::read_to_string(&abs).await.ok()
    }

    /// Computes what a file would look like after the tool runs, without writing it.
    /// Returns `None` if the result cannot be determined (e.g. edit_file on a missing file).
    pub(super) async fn compute_new_content(&self, name: &str, args: &Value) -> Option<String> {
        match name {
            "write_file" => args["content"].as_str().map(|s| s.to_string()),
            "edit_file" => {
                let path     = args["path"].as_str()?;
                let old_text = args["old"].as_str()?;
                let new_text = args["new"].as_str()?;
                let current  = self.read_current_content(path).await?;
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
                let current = self.read_current_content(path).await?;
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
                let current = self.read_current_content(path).await?;
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
}
