//! `ToolDiscovery` — records every tool actually offered to the LLM so the
//! approval / Security-groups UI can list and gate tools injected dynamically
//! outside the [`ToolRegistry`](crate::core::tools::ToolRegistry).
//!
//! Many tools reach the LLM outside the registry: `InterfaceTool` closures
//! (`write_todos`, `activate_tools`, `notify`, `show_file_to_user`, …), plugin
//! tools (Telegram's `send_voice_message`), and provider tools (`image_generate`,
//! memory backends). They are all assembled in one place —
//! [`AgentRunConfig::all_tool_defs`](crate::core::session::handler) — which is
//! the single point this service taps. Observing there *cannot drift* from what
//! is really offered, and covers every source uniformly, with zero per-component
//! wiring and no tool-name knowledge in core/plugins.
//!
//! See `docs/approval` and `docs/tools.md`.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use sqlx::SqlitePool;
use tracing::warn;

use crate::core::db::known_tools;

pub struct ToolDiscovery {
    db: Arc<SqlitePool>,
    /// Names already persisted in this process. Keeps the per-round `observe`
    /// call a cheap no-op after each tool's first sighting; DB writes happen
    /// only for genuinely-new names.
    seen: Arc<RwLock<HashSet<String>>>,
}

/// A tool extracted from an OpenAI function definition, ready to persist.
struct Draft {
    name:        String,
    description: String,
    schema:      Option<String>,
}

impl ToolDiscovery {
    pub fn new(db: Arc<SqlitePool>) -> Self {
        Self { db, seen: Arc::new(RwLock::new(HashSet::new())) }
    }

    /// Observe the full OpenAI tool array for one round. Cheap no-op once every
    /// name is known; otherwise persists the new tools in a background task so
    /// the turn is never blocked on a DB write.
    pub fn observe(&self, defs: &[Value]) {
        let new_tools: Vec<Draft> = {
            let seen = self.seen.read().unwrap();
            defs.iter().filter_map(|d| Self::extract(d, &seen)).collect()
        };
        if new_tools.is_empty() {
            return;
        }

        let db   = Arc::clone(&self.db);
        let seen = Arc::clone(&self.seen);
        tokio::spawn(async move {
            for t in new_tools {
                match known_tools::upsert(&db, &t.name, &t.description, t.schema.as_deref()).await {
                    // Mark seen only after a successful write, so a transient DB
                    // error is retried on a later round instead of lost until restart.
                    Ok(())  => { seen.write().unwrap().insert(t.name); }
                    Err(e)  => warn!(tool = %t.name, error = %e, "tool_discovery: failed to persist known tool"),
                }
            }
        });
    }

    /// Pull `(name, description, parameters)` out of an OpenAI function
    /// definition, skipping unnamed tools and ones already recorded.
    fn extract(def: &Value, seen: &HashSet<String>) -> Option<Draft> {
        let f = def.get("function")?;
        let name = f.get("name")?.as_str()?.to_string();
        if name.is_empty() || seen.contains(&name) {
            return None;
        }
        let description = f.get("description").and_then(Value::as_str).unwrap_or("").to_string();
        let schema = f.get("parameters").map(Value::to_string);
        Some(Draft { name, description, schema })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(name: &str) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("desc {name}"),
                "parameters": { "type": "object" }
            }
        })
    }

    #[test]
    fn extract_skips_unnamed_and_already_seen() {
        let mut seen = HashSet::new();
        seen.insert("write_todos".to_string());

        // Already recorded → skipped.
        assert!(ToolDiscovery::extract(&def("write_todos"), &seen).is_none());
        // Missing function.name → skipped.
        assert!(ToolDiscovery::extract(&json!({"type":"function","function":{}}), &seen).is_none());
        // New, named → extracted with name/description/schema.
        let d = ToolDiscovery::extract(&def("send_voice_message"), &seen).unwrap();
        assert_eq!(d.name, "send_voice_message");
        assert_eq!(d.description, "desc send_voice_message");
        assert!(d.schema.is_some());
    }

    fn temp_db_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        p.push(format!("skald-test-{tag}-{}-{nanos}.db", std::process::id()));
        p.to_string_lossy().into_owned()
    }

    fn cleanup(path: &str) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
    }

    /// The `observe` DB write happens in a spawned task; poll until it lands.
    async fn wait_for_rows(pool: &SqlitePool, n: usize) -> Vec<known_tools::KnownTool> {
        for _ in 0..100 {
            let rows = known_tools::all(pool).await.unwrap();
            if rows.len() >= n {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {n} known_tools rows");
    }

    #[tokio::test]
    async fn observe_persists_new_tools_and_dedups() {
        let path = temp_db_path("discovery");
        let pool = Arc::new(crate::core::db::init_pool(&path).await.unwrap());
        let disc = ToolDiscovery::new(Arc::clone(&pool));

        disc.observe(&[def("show_file_to_user"), def("notify")]);
        let rows = wait_for_rows(&pool, 2).await;
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"show_file_to_user"));
        assert!(names.contains(&"notify"));

        // Re-observing a seen tool plus a new one records only the new one.
        disc.observe(&[def("notify"), def("send_attachment")]);
        let rows = wait_for_rows(&pool, 3).await;
        assert_eq!(rows.len(), 3, "already-seen tools must not create duplicate rows");

        pool.close().await;
        cleanup(&path);
    }
}
