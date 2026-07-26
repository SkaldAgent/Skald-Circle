//! `SkaldSelector` — `ModelSelector` over `LlmManager` (blueprint §10, D14).
//!
//! The agent's required **strength is captured at construction, per-turn** —
//! the crate never sees it: `hint` carries only an explicit pin, and the AUTO
//! path delegates to `LlmManager`'s strength tiering + priority ordering.

use std::sync::Arc;

use agent_loop::activation::ToolRendering;
use agent_loop::async_trait;
use agent_loop::model::{ModelHandle, ModelHint, ModelInfo, ModelSelector};
use agent_loop::ids::ModelId;
use serde_json::Value;

use crate::llm::{DtlMode, LlmEntry, LlmManager, LlmStrength};

/// Maps Skald's per-model DTL mode to the crate's wire protocol (D15).
pub fn tool_rendering_of(dtl: DtlMode) -> ToolRendering {
    match dtl {
        DtlMode::None                    => ToolRendering::Inline,
        DtlMode::AnthropicToolReference  => ToolRendering::DeferredToolReference,
        DtlMode::KimiSystemTools         => ToolRendering::SystemToolBlock,
    }
}

/// Builds the crate-side metadata for a resolved entry. `extras` stays empty:
/// the model's `extra_params` are already baked into the client at build time
/// (they would otherwise be merged into every request body a second time).
pub fn model_info_of(entry: &LlmEntry) -> ModelInfo {
    ModelInfo {
        prompt_cache:   entry.prompt_cache,
        capabilities:   entry.capabilities.clone(),
        tool_rendering: tool_rendering_of(entry.dtl),
        extras:         Value::Null,
    }
}

/// The selector handed to the loop manager for one turn: the manager's
/// strength tiering + health + priority, behind the crate's seam.
pub struct SkaldSelector {
    manager:  Arc<LlmManager>,
    strength: Option<LlmStrength>,
}

impl SkaldSelector {
    pub fn new(manager: Arc<LlmManager>, strength: Option<LlmStrength>) -> Self {
        Self { manager, strength }
    }
}

#[async_trait]
impl ModelSelector for SkaldSelector {
    async fn select(&self, hint: &ModelHint, exclude: &[ModelId]) -> agent_loop::Result<ModelHandle> {
        let (name, entry) = if exclude.is_empty() {
            // First selection of the round: pin (hint.name) or AUTO by strength.
            self.manager.resolve(hint.name.as_deref(), self.strength).await?
        } else {
            // Fallback: next healthy model in tier/priority order, skipping the
            // ones already tried. The pin is intentionally dropped (it failed).
            let excluded: Vec<&str> = exclude.iter().map(String::as_str).collect();
            self.manager.select_excluding(&excluded, self.strength).await?
        };
        Ok(ModelHandle {
            id:    name,
            model: entry.client.clone(),
            info:  model_info_of(&entry),
        })
    }

    async fn report_success(&self, id: &ModelId) {
        self.manager.mark_success(id).await;
    }

    async fn report_failure(&self, id: &ModelId, err: &str) {
        self.manager.mark_failure(id, err).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::SqlitePool;

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

    async fn manager_with_two_models(tag: &str) -> (Arc<LlmManager>, Arc<SqlitePool>, String) {
        // Building reqwest clients (rustls-no-provider) needs the process-wide
        // crypto provider main() installs in production. Idempotent.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let path = temp_db_path(tag);
        let pool = Arc::new(crate::db::init_system_pool(&path).await.unwrap());
        sqlx::query("INSERT INTO llm_providers (id, name, type, api_key) VALUES (1, 'test', 'open_ai', 'sk-test')")
            .execute(&*pool)
            .await
            .unwrap();
        // weak: low strength, better priority; strong: high strength.
        sqlx::query("INSERT INTO llm_models (provider_id, model_id, name, strength, priority) VALUES
                     (1, 'weak-id', 'weak-model', 'low', 10),
                     (1, 'strong-id', 'strong-model', 'high', 20)")
            .execute(&*pool)
            .await
            .unwrap();

        let bus = Arc::new(core_api::system_bus::SystemEventBus::new());
        let mut registry = crate::provider::ProviderRegistry::new(bus);
        registry.register_builtin(crate::llm::providers::openai::OpenAiProvider);
        let manager = LlmManager::new(pool.clone(), Arc::new(registry), false).await.unwrap();
        (manager, pool, path)
    }

    #[tokio::test]
    async fn pin_resolves_exact_model() {
        let (manager, pool, path) = manager_with_two_models("sel-pin").await;
        let sel = SkaldSelector::new(manager, None);

        let h = sel.select(&ModelHint::name("weak-model"), &[]).await.unwrap();
        assert_eq!(h.id, "weak-model");

        assert!(sel.select(&ModelHint::name("nope"), &[]).await.is_err());

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn auto_prefers_exact_strength_then_fallback_excludes() {
        let (manager, pool, path) = manager_with_two_models("sel-auto").await;
        let sel = SkaldSelector::new(manager, Some(LlmStrength::High));

        // AUTO with strength High: the exact-tier model wins despite worse priority.
        let h = sel.select(&ModelHint::default(), &[]).await.unwrap();
        assert_eq!(h.id, "strong-model");

        // Fallback excludes it: the remaining one is served.
        let h2 = sel.select(&ModelHint::default(), &["strong-model".to_string()]).await.unwrap();
        assert_eq!(h2.id, "weak-model");

        pool.close().await;
        cleanup(&path);
    }

    #[tokio::test]
    async fn health_reporting_degrades_and_recovers() {
        let (manager, pool, path) = manager_with_two_models("sel-health").await;
        let sel = SkaldSelector::new(manager, None);

        for _ in 0..5 {
            sel.report_failure(&"weak-model".to_string(), "boom").await;
        }
        sel.report_success(&"weak-model".to_string()).await;
        // Still resolvable after recovery.
        let h = sel.select(&ModelHint::name("weak-model"), &[]).await.unwrap();
        assert_eq!(h.id, "weak-model");

        pool.close().await;
        cleanup(&path);
    }
}
