/// TranscribeManager — DB-aware registry of Speech-to-Text providers.
///
/// Two kinds of providers coexist:
/// - **DB-backed**: rows in `transcribe_models`, built from `llm_providers` credentials.
///   Managed via `add_model` / `update_model` / `delete_model`. Loaded on startup
///   and after every mutation (like `LlmManager`).
/// - **Plugin-registered**: ephemeral providers registered at runtime by plugins
///   (e.g. `WhisperLocalPlugin`). Not persisted — they disappear on plugin stop.
///
/// `get()` returns the first plugin provider if any is running, otherwise the
/// first DB-backed provider ordered by `priority ASC`.
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use sqlx::SqlitePool;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use core_api::system_bus::{SystemEvent, SystemEventBus};

use async_trait::async_trait;

use crate::core::llm::LlmProviderRecord;
use crate::core::llm::db as llm_db;
use crate::core::provider::ProviderRegistry;

use super::{Transcribe, TranscribeModelInfo, TranscribeModelRecord};
use super::db as transcribe_db;

pub use core_api::transcribe::{TranscribeProvider, TranscribeRegistry};

// ── Internal state ────────────────────────────────────────────────────────────

struct TranscribeSlot {
    record:      TranscribeModelRecord,
    provider:    LlmProviderRecord,
    transcriber: Arc<dyn Transcribe>,
}

struct ManagerState {
    /// DB-backed transcribers, ordered by priority ASC. Rebuilt on every reload().
    db_slots: Vec<TranscribeSlot>,
    /// Plugin-registered providers (ephemeral — not in DB).
    /// `WhisperLocalPlugin` registers here via `register()`.
    plugins:  Vec<Arc<dyn Transcribe>>,
}

// ── TranscribeManager ─────────────────────────────────────────────────────────

pub struct TranscribeManager {
    pool:     Arc<SqlitePool>,
    registry: Arc<ProviderRegistry>,
    state:    RwLock<ManagerState>,
}

impl TranscribeManager {
    pub async fn new(
        pool:       Arc<SqlitePool>,
        registry:   Arc<ProviderRegistry>,
        system_bus: Arc<SystemEventBus>,
        shutdown:   CancellationToken,
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

        // Reload whenever an ApiProvider is registered or unregistered.
        let weak = Arc::downgrade(&mgr);
        let mut rx = system_bus.subscribe();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        info!("transcribe_manager: reload watcher shutdown");
                        break;
                    }
                    event = rx.recv() => match event {
                        Ok(SystemEvent::ApiProviderRegistered { .. } | SystemEvent::ApiProviderUnregistered { .. }) => {
                            match weak.upgrade() {
                                Some(m) => { if let Err(e) = m.reload().await { warn!(error = %e, "transcribe_manager: reload failed"); } }
                                None    => break,
                            }
                        }
                        Ok(_) => {}
                        Err(core_api::system_bus::RecvError::Lagged(n)) => warn!(n, "transcribe_manager: system_bus lagged"),
                        Err(core_api::system_bus::RecvError::Closed)    => break,
                    }
                }
            }
        });

        Ok(mgr)
    }

    // ── Resolution ────────────────────────────────────────────────────────────

    /// Returns the first available transcriber:
    /// plugin-registered providers take precedence over DB-backed ones.
    pub async fn get(&self) -> Option<Arc<dyn Transcribe>> {
        let state = self.state.read().await;
        if let Some(p) = state.plugins.first() {
            return Some(Arc::clone(p));
        }
        state.db_slots.first().map(|s| Arc::clone(&s.transcriber))
    }

    // ── Plugin registration (ephemeral) ───────────────────────────────────────

    /// Register an ephemeral provider. Called by plugins (e.g. WhisperLocalPlugin).
    /// If a provider with the same `id()` is already present it is replaced.
    pub async fn register(&self, provider: Arc<dyn Transcribe>) {
        let mut state = self.state.write().await;
        let id = provider.id().to_string();
        state.plugins.retain(|p| p.id() != id);
        state.plugins.push(provider);
        info!(provider = %id, "transcribe provider registered (ephemeral)");
    }

    /// Deregister an ephemeral provider by id. No-op if not found.
    pub async fn unregister(&self, id: &str) {
        let mut state = self.state.write().await;
        let before = state.plugins.len();
        state.plugins.retain(|p| p.id() != id);
        if state.plugins.len() < before {
            info!(provider = %id, "transcribe provider unregistered (ephemeral)");
        }
    }

    /// Fetch the list of transcription models available from a configured provider.
    /// Returns an error if the provider doesn't support model listing.
    pub async fn list_provider_models(&self, provider_id: i64) -> Result<Vec<crate::core::transcribe::RemoteTranscribeModelInfo>> {
        let record = llm_db::load_all_providers(&self.pool).await?
            .into_iter().find(|p| p.id == provider_id)
            .ok_or_else(|| anyhow!("provider {provider_id} not found"))?;
        let provider = self.registry.get(&record.provider)
            .ok_or_else(|| anyhow!("unknown provider type '{}' for provider {provider_id}", record.provider))?;
        provider.list_transcribe_models(&record).await?
            .ok_or_else(|| anyhow!("provider '{}' does not support transcription model listing", record.name))
    }

    // ── Model CRUD (DB-backed) ────────────────────────────────────────────────

    pub async fn add_model(&self, record: TranscribeModelRecord) -> Result<i64> {
        let id = transcribe_db::insert(&self.pool, &record).await?;
        self.reload().await?;
        Ok(id)
    }

    pub async fn update_model(&self, id: i64, record: TranscribeModelRecord) -> Result<()> {
        transcribe_db::update(&self.pool, id, &record).await?;
        self.reload().await
    }

    pub async fn delete_model(&self, id: i64) -> Result<()> {
        transcribe_db::soft_delete(&self.pool, id).await?;
        self.reload().await
    }

    pub async fn get_model(&self, id: i64) -> Option<TranscribeModelRecord> {
        self.state.read().await
            .db_slots.iter()
            .find(|s| s.record.id == id)
            .map(|s| s.record.clone())
    }

    pub async fn list_models_info(&self) -> Vec<TranscribeModelInfo> {
        self.state.read().await.db_slots.iter().map(|s| TranscribeModelInfo {
            id:            s.record.id,
            provider_id:   s.provider.id,
            provider_name: s.provider.name.clone(),
            model_id:      s.record.model_id.clone(),
            name:          s.record.name.clone(),
            language:      s.record.language.clone(),
            priority:      s.record.priority,
            from_plugin:   false,
        }).collect()
    }

    /// Returns all active providers: plugin-registered first (they have precedence
    /// in `get()`), then DB-backed ordered by priority. Used by the UI.
    pub async fn list_all_info(&self) -> Vec<TranscribeModelInfo> {
        let state = self.state.read().await;

        let plugins = state.plugins.iter().map(|p| TranscribeModelInfo {
            id:            0,
            provider_id:   0,
            provider_name: "Plugin".into(),
            model_id:      p.id().to_string(),
            name:          p.id().to_string(),
            language:      None,
            priority:      0,
            from_plugin:   true,
        });

        let db = state.db_slots.iter().map(|s| TranscribeModelInfo {
            id:            s.record.id,
            provider_id:   s.provider.id,
            provider_name: s.provider.name.clone(),
            model_id:      s.record.model_id.clone(),
            name:          s.record.name.clone(),
            language:      s.record.language.clone(),
            priority:      s.record.priority,
            from_plugin:   false,
        });

        plugins.chain(db).collect()
    }

    // ── Private ───────────────────────────────────────────────────────────────

    async fn reload(&self) -> Result<()> {
        let model_records: Vec<TranscribeModelRecord> =
            transcribe_db::load_all(&self.pool).await?;
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
                        "orphaned transcribe model — provider not found, skipping",
                    );
                    continue;
                }
            };

            let result = self.registry.get(&provider.provider)
                .and_then(|p| p.build_transcriber(&provider, &model))
                .unwrap_or_else(|| anyhow::bail!("provider '{}' does not support transcription", provider.provider));
            match result {
                Ok(transcriber) => db_slots.push(TranscribeSlot { record: model, provider, transcriber }),
                Err(e) => warn!(model = %model.name, error = %e, "failed to build transcriber, skipping"),
            }
        }

        let slot_count = db_slots.len();

        // Acquire the write lock once, at the end — no more awaits after this.
        // Mirrors LlmManager::reload() to ensure the future stays Send.
        // Preserve existing plugin registrations — only replace db_slots.
        self.state.write().await.db_slots = db_slots;

        info!(db_backed = slot_count, "transcribe manager reloaded");
        Ok(())
    }
}

// ── TranscribeProvider / TranscribeRegistry impls ────────────────────────────

#[async_trait]
impl TranscribeProvider for TranscribeManager {
    async fn get(&self) -> Option<Arc<dyn Transcribe>> {
        TranscribeManager::get(self).await
    }
}

#[async_trait]
impl TranscribeRegistry for TranscribeManager {
    async fn register(&self, provider: Arc<dyn Transcribe>) {
        TranscribeManager::register(self, provider).await
    }

    async fn unregister(&self, id: &str) {
        TranscribeManager::unregister(self, id).await
    }
}

