//! `memory_search` — full-text search over the virtual memory namespace (§5).
//!
//! Unlike the fs-tools, this does **not** route a path: it searches note *content*
//! through the `memory_docs` FTS5 index (`memory_docs::search`, bm25-ranked with a
//! highlighted snippet). `user-memory` is the caller's own pool (`ToolContext::pool`);
//! `shared-memory` is the system pool captured at registration. Kept a distinct tool
//! rather than folding FTS into `grep_files`: grep is regex-per-line over a tree,
//! this is ranked keyword recall — different semantics, so different names.

use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::db::memory_docs::{self, MemoryHit};
use crate::tools::{
    SimpleExecution, Tool, ToolContext, ToolDescriptionLength, ToolExecution, ToolResult,
    truncate_label, MAX_LABEL_SHORT,
};

pub struct MemorySearch {
    /// The `shared-memory` (system) pool; see [`ReadFile`](super::ReadFile).
    shared_pool: Arc<SqlitePool>,
}

impl MemorySearch {
    pub fn new(shared_pool: Arc<SqlitePool>) -> Self { Self { shared_pool } }
}

/// Turns free text into a robust FTS5 MATCH query: each whitespace token becomes a
/// quoted term (AND-combined), so arbitrary input — colons, dashes, punctuation —
/// can't trip an FTS5 syntax error. Returns `None` when there are no tokens.
fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split_whitespace()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

fn render_hits(store: &str, hits: &[MemoryHit], out: &mut String) {
    for h in hits {
        out.push_str(&format!("[{store}] {} — {}\n", h.path, h.snippet));
    }
}

impl Tool for MemorySearch {
    fn name(&self) -> &str { "memory_search" }
    fn category(&self) -> crate::tools::ToolCategory { crate::tools::ToolCategory::Introspection }

    fn description(&self) -> &str {
        "Full-text search across your memory notes by keyword, ranked by relevance. \
         Searches user-memory/ (private) and shared-memory/ (shared); set scope to narrow it. \
         Returns matching note paths with a short highlighted snippet — open one with read_file. \
         Use this to recall where you wrote something instead of listing and reading notes one by one."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Keywords to search for. Plain words; all must appear." },
                "scope": {
                    "type": "string",
                    "enum": ["all", "private", "shared"],
                    "description": "Which store to search: 'private' (user-memory), 'shared' (shared-memory), or 'all' (default)."
                },
                "limit": { "type": "integer", "description": "Max results per store (default 10, max 50).", "default": 10 }
            },
            "required": ["query"]
        })
    }

    fn describe(&self, args: &Value, _length: ToolDescriptionLength) -> String {
        let q = args["query"].as_str().unwrap_or("?");
        truncate_label(&format!("memory_search \"{q}\""), MAX_LABEL_SHORT)
    }

    fn run_with<'a>(&'a self, ctx: &ToolContext, args: Value) -> Box<dyn ToolExecution + 'a> {
        let user_pool   = Arc::clone(&ctx.pool);
        let shared_pool = Arc::clone(&self.shared_pool);

        Box::new(SimpleExecution::new(Box::pin(async move {
            let raw = args["query"].as_str().unwrap_or("");
            let Some(q) = fts_query(raw) else {
                anyhow::bail!("memory_search needs a non-empty query");
            };
            let scope = args["scope"].as_str().unwrap_or("all");
            let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as i64;

            let mut out = String::new();
            let mut total = 0usize;
            if scope == "all" || scope == "private" {
                let hits = memory_docs::search(&user_pool, &q, limit).await?;
                total += hits.len();
                render_hits("user-memory", &hits, &mut out);
            }
            if scope == "all" || scope == "shared" {
                let hits = memory_docs::search(&shared_pool, &q, limit).await?;
                total += hits.len();
                render_hits("shared-memory", &hits, &mut out);
            }

            if total == 0 {
                return Ok(ToolResult::Text(format!("No memory notes match {raw:?}.")));
            }
            Ok(ToolResult::Text(format!("{total} result(s):\n{out}")))
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fts_query_quotes_tokens_and_survives_punctuation() {
        assert_eq!(fts_query("spesa settimana").unwrap(), "\"spesa\" \"settimana\"");
        // colons / dashes would be FTS5 operators unquoted; quoting makes them literal
        assert_eq!(fts_query("budget: 2026-07").unwrap(), "\"budget:\" \"2026-07\"");
        // an embedded quote is escaped by doubling
        assert_eq!(fts_query("say \"hi\"").unwrap(), "\"say\" \"\"\"hi\"\"\"");
        assert!(fts_query("   ").is_none());
    }
}
