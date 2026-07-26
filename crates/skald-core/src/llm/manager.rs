use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use indexmap::IndexMap;
use sqlx::SqlitePool;
use tokio::sync::RwLock;
use tracing::{info, warn};

use core_api::provider::LlmStrength;
use crate::provider::{ApiProvider, ProviderRegistry, ReasoningMode};

use super::providers::RemoteLlmModelInfo;
use super::{ClientStatus, LlmEntry, LlmModelInfo, LlmModelRecord, LlmProviderInfo, LlmProviderRecord};
use super::db;

const FAILURE_DEGRADED: u32 = 3;
const FAILURE_DOWN:     u32 = 5;
const CATALOG_TTL: Duration    = Duration::from_secs(24 * 60 * 60);
const MODEL_META_TTL: Duration = Duration::from_secs(60 * 60); // 1 hour

pub const AUTO_CLIENT: &str = "auto";

struct CachedCatalog {
    models:     Vec<RemoteLlmModelInfo>,
    fetched_at: Instant,
}

struct CachedModelMeta {
    info:       RemoteLlmModelInfo,
    fetched_at: Instant,
}

struct HealthState {
    status:               ClientStatus,
    consecutive_failures: u32,
    last_error:           Option<String>,
}

impl Default for HealthState {
    fn default() -> Self {
        Self { status: ClientStatus::Healthy, consecutive_failures: 0, last_error: None }
    }
}

struct ModelSlot {
    provider: LlmProviderRecord,
    model:    LlmModelRecord,
    entry:    Arc<LlmEntry>,
    health:   HealthState,
}

struct ManagerState {
    /// Keyed by model.name, ordered by priority ASC.
    models:   IndexMap<String, ModelSlot>,
    /// Keyed by provider.id.
    providers: IndexMap<i64, LlmProviderRecord>,
    default:  String,
}

pub struct LlmManager {
    pool:               Arc<SqlitePool>,
    registry:           Arc<ProviderRegistry>,
    state:              RwLock<ManagerState>,
    /// In-memory model catalog cache, keyed by provider_id. TTL = 24h.
    catalog:            RwLock<HashMap<i64, CachedCatalog>>,
    /// Per-model metadata cache, keyed by model display name. TTL = 1h.
    model_meta_cache:   RwLock<HashMap<String, CachedModelMeta>>,
    /// `llm.requests_log.enabled` — when `true`, a selected model is wrapped
    /// with [`crate::llm::logging::LoggingModel`] by the caller's selector
    /// (which is what knows *whose* traffic it is). See [`Self::log_pool`].
    log_enabled: bool,
}

impl LlmManager {
    pub async fn new(
        pool:        Arc<SqlitePool>,
        registry:    Arc<ProviderRegistry>,
        log_enabled: bool,
    ) -> Result<Arc<Self>> {
        let mgr = Arc::new(Self {
            pool,
            registry,
            state: RwLock::new(ManagerState {
                models:    IndexMap::new(),
                providers: IndexMap::new(),
                default:   String::new(),
            }),
            catalog:          RwLock::new(HashMap::new()),
            model_meta_cache: RwLock::new(HashMap::new()),
            log_enabled,
        });
        mgr.reload().await?;
        Ok(mgr)
    }

    // ── Public: resolution ────────────────────────────────────────────────────

    pub async fn resolve(
        &self,
        client_name:       Option<&str>,
        required_strength: Option<LlmStrength>,
    ) -> Result<(String, Arc<LlmEntry>)> {
        let name = match client_name {
            None | Some(AUTO_CLIENT) => {
                let (name, entry) = self.select(required_strength).await?;
                self.maybe_refresh_meta(&name).await;
                return Ok((name, entry));
            }
            Some(n) => {
                let state = self.state.read().await;
                if !state.models.contains_key(n) {
                    anyhow::bail!("LLM model '{n}' not found");
                }
                n.to_string()
            }
        };
        self.maybe_refresh_meta(&name).await;
        let state = self.state.read().await;
        let entry = state.models.get(&name).map(|s| s.entry.clone())
            .with_context(|| format!("LLM model '{name}' not found after refresh"))?;
        Ok((name, entry))
    }

    /// If the per-model metadata cache is stale (or missing) for `name`,
    /// fetch fresh data from the provider and update the entry if successful.
    async fn maybe_refresh_meta(&self, name: &str) {
        {
            let cache = self.model_meta_cache.read().await;
            if let Some(entry) = cache.get(name) {
                if entry.fetched_at.elapsed() < MODEL_META_TTL {
                    return;
                }
            }
        }

        let (provider_id, model_id) = {
            let state = self.state.read().await;
            match state.models.get(name) {
                Some(slot) => (slot.provider.id, slot.model.model_id.clone()),
                None => return,
            }
        };

        let remote: RemoteLlmModelInfo = match self.fetch_model_info(provider_id, &model_id).await {
            Some(m) => m,
            None => return,
        };

        let now = Instant::now();
        let mut cache = self.model_meta_cache.write().await;
        cache.insert(name.to_string(), CachedModelMeta { info: remote.clone(), fetched_at: now });

        if let Some(ctx) = remote.context_length {
            let mut state = self.state.write().await;
            if let Some(slot) = state.models.get_mut(name) {
                let old_ctx = slot.entry.context_length;
                if Some(ctx as i64) != old_ctx {
                    slot.entry = Arc::new(LlmEntry {
                        context_length: Some(ctx as i64),
                        ..(*slot.entry).clone()
                    });
                }
            }
        }
    }

    async fn fetch_model_info(&self, provider_id: i64, model_id: &str) -> Option<RemoteLlmModelInfo> {
        let record = self.state.read().await.providers.get(&provider_id).cloned()?;
        let provider = self.registry.get(&record.provider)?;
        provider.llm_model_info(&record, model_id).await.ok().flatten()
    }

    /// The registry pool the request log lands in, or `None` when logging is
    /// disabled (`llm.requests_log.enabled: false`). A selector wraps the model
    /// it hands out only when this is `Some`.
    pub fn log_pool(&self) -> Option<Arc<SqlitePool>> {
        self.log_enabled.then(|| Arc::clone(&self.pool))
    }

    pub async fn get(&self, name: &str) -> Option<Arc<LlmEntry>> {
        self.state.read().await.models.get(name).map(|s| s.entry.clone())
    }

    pub async fn default_name(&self) -> String {
        self.state.read().await.default.clone()
    }

    /// Returns ["auto", <model1>, <model2>, …] for the frontend selector.
    pub async fn client_names(&self) -> Vec<String> {
        let mut names = vec![AUTO_CLIENT.to_string()];
        names.extend(self.state.read().await.models.keys().cloned());
        names
    }

    // ── Public: health reporting ──────────────────────────────────────────────

    pub async fn mark_success(&self, name: &str) {
        let mut state = self.state.write().await;
        if let Some(slot) = state.models.get_mut(name) {
            let h = &mut slot.health;
            if h.consecutive_failures > 0 {
                info!(model = name, "LLM model recovered");
            }
            h.consecutive_failures = 0;
            h.last_error           = None;
            h.status               = ClientStatus::Healthy;
        }
    }

    pub async fn mark_failure(&self, name: &str, error: &str) {
        let mut state = self.state.write().await;
        if let Some(slot) = state.models.get_mut(name) {
            let h = &mut slot.health;
            h.consecutive_failures += 1;
            h.last_error = Some(error.to_string());
            h.status = if h.consecutive_failures >= FAILURE_DOWN {
                warn!(model = name, failures = h.consecutive_failures, "LLM model marked DOWN");
                ClientStatus::Down
            } else if h.consecutive_failures >= FAILURE_DEGRADED {
                warn!(model = name, failures = h.consecutive_failures, "LLM model marked DEGRADED");
                ClientStatus::Degraded
            } else {
                ClientStatus::Healthy
            };
        }
    }

    // ── Public: provider CRUD ─────────────────────────────────────────────────

    pub async fn add_provider(&self, record: LlmProviderRecord) -> Result<i64> {
        let id = db::insert_provider(&self.pool, &record).await?;
        self.reload().await?;
        Ok(id)
    }

    pub async fn update_provider(&self, id: i64, record: LlmProviderRecord) -> Result<()> {
        db::update_provider(&self.pool, id, &record).await?;
        self.reload().await
    }

    pub async fn delete_provider(&self, id: i64) -> Result<()> {
        db::delete_provider(&self.pool, id).await?;
        self.reload().await
    }

    pub async fn get_provider(&self, id: i64) -> Option<LlmProviderRecord> {
        self.state.read().await.providers.get(&id).cloned()
    }

    /// Returns the ApiProvider implementation for the given provider record id.
    pub async fn get_api_provider(&self, id: i64) -> Option<Arc<dyn ApiProvider>> {
        let record = self.state.read().await.providers.get(&id).cloned()?;
        self.registry.get(&record.provider)
    }

    /// Returns the remote model catalog for a provider, using a 24h in-memory cache.
    /// After fetching, syncs context/token/capability metadata to existing DB model records.
    pub async fn list_provider_models(&self, id: i64) -> Result<Vec<RemoteLlmModelInfo>> {
        {
            let cache = self.catalog.read().await;
            if let Some(entry) = cache.get(&id) {
                if entry.fetched_at.elapsed() < CATALOG_TTL {
                    return Ok(entry.models.clone());
                }
            }
        }

        let record = self.state.read().await.providers.get(&id).cloned()
            .ok_or_else(|| anyhow::anyhow!("provider {id} not found"))?;
        let provider = self.registry.get(&record.provider)
            .ok_or_else(|| anyhow::anyhow!("unknown provider type '{}' for provider {id}", record.provider))?;

        let mut models = provider.list_llm_models(&record).await?
            .ok_or_else(|| anyhow::anyhow!("this provider does not support model listing"))?;

        // Fill the reasoning descriptor per catalog model for the add-from-catalog
        // UI. Providers that already populate a precise descriptor in their
        // listing (e.g. OpenRouter from each model's `reasoning` object) keep it;
        // the rest fall back to the capability-based `reasoning_mode`.
        for m in &mut models {
            if m.reasoning.is_none() {
                m.reasoning = provider.reasoning_mode(&m.id, &m.capabilities);
            }
        }

        for remote in &models {
            db::update_model_metadata(
                &self.pool, id, &remote.id,
                remote.context_length.map(|v| v as i64),
                remote.max_completion_tokens.map(|v| v as i64),
                remote.knowledge_cutoff.as_deref(),
                &remote.capabilities,
            ).await.ok();
        }

        self.catalog.write().await.insert(id, CachedCatalog {
            models:     models.clone(),
            fetched_at: Instant::now(),
        });

        Ok(models)
    }

    pub async fn list_providers_info(&self) -> Vec<LlmProviderInfo> {
        self.state.read().await.providers.values().map(|p| {
            let supported_types = self.registry.get(&p.provider)
                .map(|prov| prov.supported_types().to_vec())
                .unwrap_or_default();
            LlmProviderInfo {
                id:              p.id,
                name:            p.name.clone(),
                provider:        p.provider.clone(),
                base_url:        p.base_url.clone(),
                description:     p.description.clone(),
                supported_types,
            }
        }).collect()
    }

    // ── Public: model CRUD ────────────────────────────────────────────────────

    pub async fn add_model(&self, model: LlmModelRecord) -> Result<i64> {
        if model.is_default {
            db::clear_default(&self.pool).await?;
        }
        let id = db::insert_model(&self.pool, &model).await?;
        self.reload().await?;
        Ok(id)
    }

    pub async fn update_model(&self, id: i64, model: LlmModelRecord) -> Result<()> {
        if model.is_default {
            db::clear_default(&self.pool).await?;
        }
        db::update_model(&self.pool, id, &model).await?;
        self.reload().await
    }

    pub async fn delete_model(&self, id: i64) -> Result<()> {
        db::delete_model(&self.pool, id).await?;
        self.reload().await
    }

    pub async fn get_model(&self, id: i64) -> Option<LlmModelRecord> {
        self.state.read().await.models.values()
            .find(|s| s.model.id == id)
            .map(|s| s.model.clone())
    }

    /// Reasoning control descriptor for a (provider, model_id) pair — used by the
    /// "add model" form to render the right control before the model is saved.
    /// Capabilities are unknown at this point (manual entry), so an empty slice
    /// is passed; catalog-based flows use the descriptor attached to each model.
    pub async fn reasoning_mode_for(&self, provider_id: i64, model_id: &str) -> Option<ReasoningMode> {
        let record   = self.state.read().await.providers.get(&provider_id).cloned()?;
        let provider = self.registry.get(&record.provider)?;
        provider.reasoning_mode(model_id, &[])
    }

    pub async fn list_models_info(&self) -> Vec<LlmModelInfo> {
        let state   = self.state.read().await;
        let catalog = self.catalog.read().await;
        state.models.values().map(|slot| {
            let cached = catalog.get(&slot.provider.id)
                .and_then(|c| c.models.iter().find(|m| m.id == slot.model.model_id));
            let reasoning_mode = self.registry.get(&slot.provider.provider)
                .and_then(|p| p.reasoning_mode(&slot.model.model_id, &slot.model.capabilities));
            LlmModelInfo {
                id:                       slot.model.id,
                provider_id:              slot.provider.id,
                provider_name:            slot.provider.name.clone(),
                model_id:                 slot.model.model_id.clone(),
                name:                     slot.model.name.clone(),
                strength:                 slot.model.strength,
                is_default:               slot.model.is_default,
                priority:                 slot.model.priority,
                extra_params:             slot.model.extra_params.clone(),
                context_length:           slot.model.context_length,
                max_output_tokens:        slot.model.max_output_tokens,
                knowledge_cutoff:         slot.model.knowledge_cutoff.clone(),
                capabilities:             slot.model.capabilities.clone(),
                status:                   slot.health.status,
                last_error:               slot.health.last_error.clone(),
                price_input_per_million:  cached.and_then(|m| m.price_input_per_million),
                price_output_per_million: cached.and_then(|m| m.price_output_per_million),
                reasoning:                slot.model.reasoning.clone(),
                reasoning_mode,
            }
        }).collect()
    }

    // ── Public: selection ─────────────────────────────────────────────────────

    pub async fn select_excluding(
        &self,
        excluded:          &[&str],
        required_strength: Option<LlmStrength>,
    ) -> Result<(String, Arc<LlmEntry>)> {
        let state = self.state.read().await;
        let mut slots: Vec<(&String, &ModelSlot)> = state.models.iter()
            .filter(|(name, _)| !excluded.contains(&name.as_str()))
            .collect();
        if slots.is_empty() {
            anyhow::bail!("no alternative LLM models available");
        }
        sort_slots_for_agent(&mut slots, required_strength);
        if let Some((name, slot)) = slots.iter().find(|(_, s)| s.health.status != ClientStatus::Down) {
            return Ok((name.to_string(), slot.entry.clone()));
        }
        if let Some((name, slot)) = slots.first() {
            warn!(model = %name, "all alternative LLM models are DOWN — using best available");
            return Ok((name.to_string(), slot.entry.clone()));
        }
        anyhow::bail!("no alternative LLM models available");
    }

    async fn select(
        &self,
        required_strength: Option<LlmStrength>,
    ) -> Result<(String, Arc<LlmEntry>)> {
        let state = self.state.read().await;

        if state.models.is_empty() {
            anyhow::bail!("no LLM models configured — add one via the UI");
        }

        let mut slots: Vec<(&String, &ModelSlot)> = state.models.iter().collect();
        sort_slots_for_agent(&mut slots, required_strength);

        if let Some((name, slot)) = slots.iter().find(|(_, s)| s.health.status != ClientStatus::Down) {
            return Ok((name.to_string(), slot.entry.clone()));
        }

        if let Some((name, slot)) = slots.first() {
            warn!(model = %name, "all LLM models are DOWN — using strongest as emergency fallback");
            return Ok((name.to_string(), slot.entry.clone()));
        }

        anyhow::bail!("no LLM models available");
    }

    // ── Private ───────────────────────────────────────────────────────────────

    async fn reload(&self) -> Result<()> {
        let provider_records = db::load_all_providers(&self.pool).await?;
        let model_records    = db::load_all_models(&self.pool).await?;

        let providers: IndexMap<i64, LlmProviderRecord> = provider_records
            .into_iter()
            .map(|p| (p.id, p))
            .collect();

        let mut models: IndexMap<String, ModelSlot> = IndexMap::new();
        let mut default = String::new();

        for model in model_records {
            let provider = match providers.get(&model.provider_id) {
                Some(p) => p.clone(),
                None    => {
                    warn!(model = %model.name, provider_id = model.provider_id, "orphaned model — provider not found, skipping");
                    continue;
                }
            };

            let entry = match build_entry(&self.registry, &provider, &model, model.id) {
                Ok(e)  => Arc::new(e),
                Err(e) => {
                    warn!(model = %model.name, error = %e, "failed to build LLM entry, skipping");
                    continue;
                }
            };

            if model.is_default || default.is_empty() {
                default = model.name.clone();
            }

            models.insert(model.name.clone(), ModelSlot {
                provider,
                model,
                entry,
                health: HealthState::default(),
            });
        }

        let mut state = self.state.write().await;
        for (name, slot) in state.models.iter() {
            if let Some(new_slot) = models.get_mut(name) {
                new_slot.health.status               = slot.health.status;
                new_slot.health.consecutive_failures = slot.health.consecutive_failures;
                new_slot.health.last_error           = slot.health.last_error.clone();
            }
        }
        state.models    = models;
        state.providers = providers;
        state.default   = default;
        Ok(())
    }
}

// ── Builder ───────────────────────────────────────────────────────────────────

fn build_entry(
    registry:   &ProviderRegistry,
    provider:   &LlmProviderRecord,
    model:      &LlmModelRecord,
    model_db_id: i64,
) -> Result<LlmEntry> {
    let built = registry.get(&provider.provider)
        .ok_or_else(|| anyhow::anyhow!("unknown provider type '{}'", provider.provider))?
        .build_llm(provider, model)
        .ok_or_else(|| anyhow::anyhow!("provider '{}' does not support LLM", provider.provider))??;

    // The bare client: request logging is a per-caller decorator applied by the
    // `ModelSelector` (it is the only place that knows the owner), not here.
    let client       = built.client;
    let prompt_cache = built.prompt_cache;
    let extra        = model.extra_params.clone();

    Ok(LlmEntry {
        client,
        model:          model.model_id.clone(),
        model_db_id,
        strength:       model.strength,
        extra_params:   extra,
        context_length: model.context_length,
        prompt_cache,
        capabilities:   model.capabilities.clone(),
        // DTL is opt-in per model (the `tool_search` capability); the wire *format*
        // comes from the model's provider (native, or `providers.yaml`) — no
        // hardcoded model list.
        dtl:            if model.capabilities.iter().any(|c| c == "tool_search") {
            registry.get(&provider.provider)
                .and_then(|p| p.dtl_format().map(crate::llm::dtl_mode_from_format))
                .unwrap_or(crate::llm::DtlMode::None)
        } else {
            crate::llm::DtlMode::None
        },
    })
}

// ── Sorting helpers ───────────────────────────────────────────────────────────

pub fn sort_models_for_agent(
    mut models: Vec<LlmModelInfo>,
    strength:   Option<LlmStrength>,
) -> Vec<LlmModelInfo> {
    models.sort_by_key(|m| (model_tier(m.strength, strength), m.priority));
    models
}

fn sort_slots_for_agent(
    slots:    &mut Vec<(&String, &ModelSlot)>,
    strength: Option<LlmStrength>,
) {
    slots.sort_by_key(|(_, s)| (
        model_tier(s.model.strength, strength),
        s.model.priority,
    ));
}

fn model_tier(
    model_strength: Option<LlmStrength>,
    req_strength:   Option<LlmStrength>,
) -> u8 {
    let strength_ok = match (req_strength, model_strength) {
        (Some(req), Some(avail)) => avail >= req,
        (Some(_), None)          => false,
        (None, _)                => true,
    };
    // Prefer exact strength match over over-qualified models so that e.g. an
    // agent with strength=low picks the `low` model before `average`.
    let exact_match = match (req_strength, model_strength) {
        (Some(req), Some(avail)) => avail == req,
        _                        => true,
    };
    match (strength_ok, exact_match) {
        (true, true)  => 0, // exact strength
        (true, false) => 1, // over-qualified
        _             => 3, // doesn't meet minimum bar
    }
}
