/// ImageGeneratorManager — DB-aware registry of image generation providers.
///
/// Two kinds of providers coexist:
/// - **DB-backed**: rows in `image_generate_models`, built from `llm_providers` credentials.
///   Managed via `add_model` / `update_model` / `delete_model`. Loaded on startup
///   and after every mutation.
/// - **Plugin-registered**: ephemeral providers registered at runtime by plugins.
///   Not persisted — they disappear on plugin stop.
///
/// `get(id)` resolves by explicit id across both plugin and DB-backed providers.
/// When called without an id, plugin providers take precedence over DB-backed ones.
use std::sync::Arc;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use sqlx::SqlitePool;
use tokio::sync::RwLock;
use tracing::{info, warn};

use core_api::image_generate::ImageGenerateRegistry;

use crate::llm::LlmProviderRecord;
use crate::llm::db as llm_db;
use crate::provider::ProviderRegistry;
use crate::tools::Tool;

use super::{ImageGenerate, ImageGenerateInfo, ImageGenerateModelInfo, ImageGenerateModelRecord};
use super::db as image_db;

// ── Internal state ────────────────────────────────────────────────────────────

struct ImageGenerateSlot {
    record:    ImageGenerateModelRecord,
    provider:  LlmProviderRecord,
    generator: Arc<dyn ImageGenerate>,
}

struct ManagerState {
    /// DB-backed generators, ordered by priority ASC. Rebuilt on every reload().
    db_slots: Vec<ImageGenerateSlot>,
    /// Plugin-registered providers (ephemeral — not in DB).
    plugins:  Vec<Arc<dyn ImageGenerate>>,
}

// ── ImageGeneratorManager ─────────────────────────────────────────────────────

pub struct ImageGeneratorManager {
    pool:      Arc<SqlitePool>,
    registry:  Arc<ProviderRegistry>,
    state:     RwLock<ManagerState>,
}

impl ImageGeneratorManager {
    pub async fn new(
        pool:     Arc<SqlitePool>,
        registry: Arc<ProviderRegistry>,
    ) -> Result<Arc<Self>> {
        let mgr = Arc::new(Self {
            pool,
            registry,
            state: RwLock::new(ManagerState {
                db_slots: Vec::new(),
                plugins:  Vec::new(),
            }),
        });
        mgr.reload().await?;
        Ok(mgr)
    }

    // ── Plugin registration (ephemeral) ───────────────────────────────────────

    pub async fn register(&self, provider: Arc<dyn ImageGenerate>) {
        let mut state = self.state.write().await;
        let id = provider.id().to_string();
        state.plugins.retain(|p| p.id() != id);
        state.plugins.push(provider);
        info!(provider_id = %id, "image generator registered (plugin)");
    }

    pub async fn unregister(&self, id: &str) {
        let mut state = self.state.write().await;
        let before = state.plugins.len();
        state.plugins.retain(|p| p.id() != id);
        if state.plugins.len() < before {
            info!(provider_id = %id, "image generator unregistered (plugin)");
        }
    }

    // ── Model CRUD (DB-backed) ────────────────────────────────────────────────

    pub async fn add_model(&self, record: ImageGenerateModelRecord) -> Result<i64> {
        let id = image_db::insert(&self.pool, &record).await?;
        self.reload().await?;
        Ok(id)
    }

    pub async fn update_model(&self, id: i64, record: ImageGenerateModelRecord) -> Result<()> {
        image_db::update(&self.pool, id, &record).await?;
        self.reload().await
    }

    pub async fn delete_model(&self, id: i64) -> Result<()> {
        image_db::soft_delete(&self.pool, id).await?;
        self.reload().await
    }

    pub async fn get_model(&self, id: i64) -> Option<ImageGenerateModelRecord> {
        self.state.read().await
            .db_slots.iter()
            .find(|s| s.record.id == id)
            .map(|s| s.record.clone())
    }

    pub async fn list_models_info(&self) -> Vec<ImageGenerateModelInfo> {
        self.state.read().await.db_slots.iter().map(|s| ImageGenerateModelInfo {
            id:            s.record.id,
            provider_id:   s.provider.id,
            provider_name: s.provider.name.clone(),
            model_id:      s.record.model_id.clone(),
            name:          s.record.name.clone(),
            priority:      s.record.priority,
            from_plugin:   false,
            description:   None,
        }).collect()
    }

    /// Returns all active providers: plugin-registered first, then DB-backed by priority.
    pub async fn list_all_info(&self) -> Vec<ImageGenerateModelInfo> {
        let state = self.state.read().await;

        let plugins = state.plugins.iter().map(|p| ImageGenerateModelInfo {
            id:            0,
            provider_id:   0,
            provider_name: "Plugin".into(),
            model_id:      p.id().to_string(),
            name:          p.name().to_string(),
            priority:      0,
            from_plugin:   true,
            description:   p.description().map(str::to_string),
        });

        let db = state.db_slots.iter().map(|s| ImageGenerateModelInfo {
            id:            s.record.id,
            provider_id:   s.provider.id,
            provider_name: s.provider.name.clone(),
            model_id:      s.record.model_id.clone(),
            name:          s.record.name.clone(),
            priority:      s.record.priority,
            from_plugin:   false,
            description:   None,
        });

        plugins.chain(db).collect()
    }

    // ── Provider queries ───────────────────────────────────────────────────────

    /// Returns all active providers as lightweight info structs (for LLM tool).
    pub async fn list(&self) -> Vec<ImageGenerateInfo> {
        let state = self.state.read().await;
        state.plugins.iter()
            .map(|p| ImageGenerateInfo {
                id:                  p.id().to_string(),
                name:                p.name().to_string(),
                description:         p.description().map(str::to_string),
                extra_params_schema: p.extra_params_schema(),
            })
            .chain(state.db_slots.iter().map(|s| ImageGenerateInfo {
                id:                  s.record.name.clone(),
                name:                s.record.name.clone(),
                description:         None,
                extra_params_schema: None,
            }))
            .collect()
    }

    /// Looks up a provider by id — plugins first, then DB-backed by name.
    pub async fn get(&self, id: &str) -> Option<Arc<dyn ImageGenerate>> {
        let state = self.state.read().await;
        if let Some(p) = state.plugins.iter().find(|p| p.id() == id) {
            return Some(Arc::clone(p));
        }
        state.db_slots.iter()
            .find(|s| s.record.name == id)
            .map(|s| Arc::clone(&s.generator))
    }

    // ── Generation ────────────────────────────────────────────────────────────

    /// Renders `prompt` with `provider_id` and hands the raw bytes back.
    ///
    /// **Placement is the caller's**, deliberately. This used to write the file
    /// into the server's own `data/images/` and return that host path to
    /// the model — a path in nobody's vocabulary: it is not the caller's home,
    /// not their container, and every consumer downstream resolves agent paths
    /// (§6). Telegram's `send_attachment` therefore looked for
    /// `data/images/x.png` under the user's home and answered "file not found",
    /// and `read_file`/`execute_cmd`/the viewer could not reach it either. The
    /// manager has no `UserFs` and no session, so the one place that does — the
    /// tool, through its `ToolContext` — owns where the image lands.
    pub async fn generate_bytes(
        &self,
        provider_id:  &str,
        prompt:       &str,
        extra_params: Option<&serde_json::Value>,
    ) -> Result<Vec<u8>> {
        let provider = self.get(provider_id).await
            .ok_or_else(|| anyhow!("image provider '{}' not found", provider_id))?;

        let bytes = provider.generate(prompt, extra_params).await?;
        info!(provider_id, bytes = bytes.len(), "image generated");

        Ok(bytes)
    }

    // ── Tool injection ─────────────────────────────────────────────────────────

    /// Returns the two image tools when at least one provider is active.
    /// Called per-turn by the session handler to conditionally inject tools.
    pub async fn tools(self: Arc<Self>) -> Vec<Arc<dyn Tool>> {
        let state = self.state.read().await;
        if state.plugins.is_empty() && state.db_slots.is_empty() {
            return vec![];
        }
        drop(state);
        vec![
            Arc::new(crate::tools::image_generate::ImageGenerateProvidersList { mgr: Arc::clone(&self) }) as Arc<dyn Tool>,
            Arc::new(crate::tools::image_generate::ImageGenerateTool          { mgr: Arc::clone(&self) }) as Arc<dyn Tool>,
        ]
    }

    // ── Private ───────────────────────────────────────────────────────────────

    async fn reload(&self) -> Result<()> {
        let model_records = image_db::load_all(&self.pool).await?;
        let provider_records: Vec<LlmProviderRecord> =
            llm_db::load_all_providers(&self.pool).await?;

        let providers: std::collections::HashMap<i64, LlmProviderRecord> =
            provider_records.into_iter().map(|p| (p.id, p)).collect();

        let mut db_slots = Vec::new();

        for model in model_records {
            let provider = match providers.get(&model.provider_id) {
                Some(p) => p.clone(),
                None => {
                    warn!(
                        model = %model.name,
                        provider_id = model.provider_id,
                        "orphaned image model — provider not found, skipping",
                    );
                    continue;
                }
            };

            let result = self.registry.get(&provider.provider)
                .and_then(|p| p.build_image_generator(&provider, &model))
                .unwrap_or_else(|| anyhow::bail!("provider '{}' does not support image generation", provider.provider));
            match result {
                Ok(generator) => db_slots.push(ImageGenerateSlot { record: model, provider, generator }),
                Err(e) => warn!(model = %model.name, error = %e, "failed to build image generator, skipping"),
            }
        }

        let slot_count = db_slots.len();
        self.state.write().await.db_slots = db_slots;
        info!(db_backed = slot_count, "image generator manager reloaded");
        Ok(())
    }
}


// ── ImageGenerateRegistry impl ────────────────────────────────────────────────

#[async_trait]
impl ImageGenerateRegistry for ImageGeneratorManager {
    async fn register(&self, provider: Arc<dyn ImageGenerate>) {
        ImageGeneratorManager::register(self, provider).await;
    }

    async fn unregister(&self, id: &str) {
        ImageGeneratorManager::unregister(self, id).await;
    }
}
