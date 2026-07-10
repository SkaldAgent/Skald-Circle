//! `known_tools` — every tool ever offered to the LLM, captured at injection
//! time by [`crate::core::tool_discovery::ToolDiscovery`].
//!
//! This is the drift-proof half of tool visibility: instead of maintaining a
//! parallel list of "all tools", we record what is actually assembled into the
//! LLM request (`AgentRunConfig::all_tool_defs`). The approval / Security-groups
//! UI merges these rows so tools injected outside the `ToolRegistry` (interface
//! tools, plugin tools, provider tools) can still be assigned a permission.

use anyhow::Result;
use serde::Serialize;
use sqlx::SqlitePool;

#[derive(Debug, Clone, Serialize)]
pub struct KnownTool {
    pub name:        String,
    pub description: String,
    /// JSON parameters schema as last seen, if any.
    pub schema:      Option<String>,
}

/// Records (or refreshes) a tool by name. Idempotent: re-seeing a tool updates
/// its description/schema and bumps `last_seen`.
pub async fn upsert(
    pool:        &SqlitePool,
    name:        &str,
    description: &str,
    schema:      Option<&str>,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO known_tools (name, description, schema, first_seen, last_seen)
         VALUES (?1, ?2, ?3, strftime('%s','now'), strftime('%s','now'))
         ON CONFLICT(name) DO UPDATE SET
             description = excluded.description,
             schema      = excluded.schema,
             last_seen   = excluded.last_seen",
    )
    .bind(name)
    .bind(description)
    .bind(schema)
    .execute(pool)
    .await?;
    Ok(())
}

/// All recorded tools, sorted by name.
pub async fn all(pool: &SqlitePool) -> Result<Vec<KnownTool>> {
    let rows = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT name, description, schema FROM known_tools ORDER BY name",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(name, description, schema)| KnownTool { name, description, schema })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn upsert_is_idempotent_and_updates_metadata() {
        let path = temp_db_path("known-tools");
        let pool = crate::core::db::init_pool(&path).await.unwrap();

        upsert(&pool, "send_voice_message", "v1", Some(r#"{"type":"object"}"#)).await.unwrap();
        upsert(&pool, "send_voice_message", "v2", None).await.unwrap();

        let rows = all(&pool).await.unwrap();
        assert_eq!(rows.len(), 1, "same name must not create a second row");
        assert_eq!(rows[0].name, "send_voice_message");
        assert_eq!(rows[0].description, "v2", "description is refreshed on re-upsert");
        assert_eq!(rows[0].schema, None, "schema is refreshed on re-upsert");

        pool.close().await;
        cleanup(&path);
    }
}
