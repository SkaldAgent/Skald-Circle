//! Context compaction — reduces LLM context size by summarising old messages.
//!
//! # Responsibility
//! [`ContextCompactor`] is Skald's **policy**: when to compact (the token
//! threshold, the ephemeral guard), which model summarises, and telling the
//! rest of the app it happened. The mechanics — split point, transcript,
//! prompt, the summariser call, the saved row — are the library's
//! (`agent_loop::compaction`), so a compaction is the same operation whether
//! Skald or another host triggers it.
//!
//! It is a stateless service (all state lives in the DB), shared via `Arc`
//! across every [`ChatSessionHandler`](crate::session::handler). Triggered at
//! the **start of a turn** when the previous turn's `input_tokens` exceeded the
//! threshold, or manually via `force_compact`. Ephemeral sessions (cron, event-triage)
//! are always skipped.
//!
//! ```text
//! handle_message()
//!   └─► ContextCompactor::try_compact(manager, …, last_input_tokens)
//!         ├─ guard: is_ephemeral                     → Ok(false)
//!         ├─ guard: tokens (or estimate) < threshold → Ok(false)
//!         └─► manager.new_compaction(conv, frame).run()
//!               ├─ split at the keep_recent boundary, on a user/agent message
//!               ├─ summarise (one call, no tools)
//!               ├─ save the summary row
//!               └─ hooks.on_compacted → DTL re-anchor (loop_adapters::hooks)
//! ```
//!
//! The next turn needs nothing from this: the assembler reads the latest
//! summary from the store and projects it in front of the surviving messages.

use std::sync::Arc;

use agent_loop::compaction::{CompactionMode, should_compact};
use agent_loop::manager::LoopManager;
use agent_loop::model::ModelHint;
use sqlx::SqlitePool;
use tracing::{info, warn};

use core_api::{ConfigProperty, ConfigSet, PropertyType};

use crate::chat_event_bus::{ChatEventBus, CompactionEvent};
use crate::config::CompactionConfig;
use crate::config_store::GlobalConfigManager;
use crate::db::chat_history;
use crate::llm::LlmManager;
use crate::llm::logging::RequestLogTarget;
use crate::loop_adapters::history::SqliteHistory;
use crate::loop_adapters::selector::SkaldSelector;

/// Registry `config` key holding the name of the LLM model to use for
/// compaction summaries. Set from the Settings page (instance-wide); empty /
/// unset means AUTO selection by `CompactionConfig.strength` (config.yml).
pub const COMPACTION_MODEL_KEY: &str = "compaction_model";

/// Settings-page section for compaction (see `i18n::config_set` for the
/// pattern). Registered in `Runtime::config_properties`.
pub fn config_set() -> ConfigSet {
    ConfigSet {
        name:        "Compaction".into(),
        description: "How conversation history is summarised when the context grows too large.".into(),
        properties:  vec![
            ConfigProperty {
                key:           COMPACTION_MODEL_KEY.into(),
                name:          "Compaction model".into(),
                description:   "Model used to summarise compacted history, for the whole instance. \
                                A cheap model is usually enough. Leave empty to auto-select \
                                (by `compaction.strength` in config.yml).".into(),
                property_type: PropertyType::LlmModel,
                default_value: None,
            },
        ],
        owner:       None,
    }
}

// ── The summariser's wording ─────────────────────────────────────────────────

/// Prefix prepended to a stored summary when it is projected back into the
/// context. Re-exported from the library, which owns the wording along with the
/// preamble and the section template: the assembler on the other side of the
/// projection reads the same constant, so the two can never drift.
pub use agent_loop::compaction::SUMMARY_PREFIX;


// ── Public API ────────────────────────────────────────────────────────────────

pub struct ContextCompactor {
    config:       CompactionConfig,
    llm_manager:  Arc<LlmManager>,
    event_bus:    Arc<ChatEventBus>,
    config_store: Arc<GlobalConfigManager>,
}

impl ContextCompactor {
    pub fn new(
        config:       CompactionConfig,
        llm_manager:  Arc<LlmManager>,
        event_bus:    Arc<ChatEventBus>,
        config_store: Arc<GlobalConfigManager>,
    ) -> Self {
        Self { config, llm_manager, event_bus, config_store }
    }

    /// Whether the **automatic** trigger is armed. The compactor always exists
    /// (manual `/compact` needs no config), so this — not its presence — is what
    /// tells the projection that a summary bounds the context.
    pub fn auto_enabled(&self) -> bool {
        self.config.threshold_tokens.is_some()
    }

    /// Attempt to compact the conversation history for `stack_id`.
    ///
    /// * `last_input_tokens` — input tokens from the **previous** turn.
    ///   Pass `0` when the provider did not report usage (a character-count
    ///   estimate is used as fallback in that case).
    /// * `is_ephemeral` — skip compaction for short-lived automated sessions.
    ///
    /// Returns `true` if a new summary was written, `false` if skipped.
    pub async fn try_compact(
        &self,
        manager:           &Arc<LoopManager>,
        pool:              &Arc<SqlitePool>,
        user_id:           &str,
        session_id:        i64,
        stack_id:          i64,
        last_input_tokens: u32,
        is_ephemeral:      bool,
    ) -> anyhow::Result<bool> {
        if is_ephemeral {
            return Ok(false);
        }
        // No threshold configured ⇒ automatic compaction is off and history stays
        // append-only. `force_compact` deliberately does not consult this: the
        // human asking for `/compact` *is* the trigger.
        let Some(threshold) = self.config.threshold_tokens else {
            return Ok(false);
        };

        // A provider that reported no usage leaves only the character estimate.
        let estimated = chat_history::estimate_tokens_for_stack(pool, stack_id).await?;
        if !should_compact(Some(last_input_tokens), estimated, threshold) {
            return Ok(false);
        }
        let effective_tokens = if last_input_tokens > 0 { last_input_tokens } else { estimated };

        info!(
            stack_id,
            effective_tokens,
            threshold,
            "compactor: threshold exceeded, starting compaction"
        );

        self.do_compact(manager, pool, user_id, session_id, stack_id, effective_tokens).await
    }

    /// Force compaction regardless of the token threshold.
    /// Still respects the ephemeral guard.
    ///
    /// Returns `true` if a new summary was written, `false` if skipped.
    pub async fn force_compact(
        &self,
        manager:      &Arc<LoopManager>,
        pool:         &Arc<SqlitePool>,
        user_id:      &str,
        session_id:   i64,
        stack_id:     i64,
        is_ephemeral: bool,
    ) -> anyhow::Result<bool> {
        if is_ephemeral {
            return Ok(false);
        }

        let effective_tokens = chat_history::estimate_tokens_for_stack(pool, stack_id).await?;
        info!(
            stack_id,
            effective_tokens,
            "compactor: manual compaction triggered"
        );

        self.do_compact(manager, pool, user_id, session_id, stack_id, effective_tokens).await
    }

    /// Runs the library's compaction on the frame with Skald's model policy,
    /// then publishes the result on the app's event bus.
    ///
    /// Model: the instance-wide Settings pick (`compaction_model`) wins; empty,
    /// unset, or naming a model that no longer exists all degrade to AUTO
    /// selection by `compaction.strength` from config.yml.
    #[allow(clippy::too_many_arguments)]
    async fn do_compact(
        &self,
        manager:          &Arc<LoopManager>,
        pool:             &Arc<SqlitePool>,
        user_id:          &str,
        session_id:       i64,
        stack_id:         i64,
        effective_tokens: u32,
    ) -> anyhow::Result<bool> {
        let hint = self.model_hint().await;
        let conv = SqliteHistory::conversation(session_id);

        let outcome = manager
            .new_compaction(conv, agent_loop::ids::FrameId(stack_id))
            .mode(CompactionMode::Auto { keep_tail: self.config.keep_recent })
            // Strength is Skald's, captured here (D14): a pin bypasses it. The
            // owner rides along so the summariser's call shows up in the
            // requests log like any other (session/frame come from the request).
            .selector(Arc::new(
                SkaldSelector::new(Arc::clone(&self.llm_manager), self.config.strength)
                    .with_log(RequestLogTarget::user(user_id, Arc::clone(pool))),
            ))
            .model(hint)
            .run()
            .await?;

        let Some(outcome) = outcome else { return Ok(false) };

        self.event_bus.compaction_done(CompactionEvent {
            session_id,
            stack_id,
            summary_id:              outcome.summary_id.get(),
            covers_up_to_message_id: outcome.covered_up_to.get(),
            triggered_by_tokens:     effective_tokens,
        });
        Ok(true)
    }

    /// The summariser's model pin, or `ModelHint::default()` (AUTO) when none is
    /// configured or the configured one is gone.
    async fn model_hint(&self) -> ModelHint {
        let configured = self
            .config_store
            .get(COMPACTION_MODEL_KEY)
            .await
            .ok()
            .flatten()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let Some(name) = configured else { return ModelHint::default() };

        match self.llm_manager.resolve(Some(&name), None).await {
            Ok((resolved, _)) => ModelHint::name(resolved),
            Err(e) => {
                warn!(model = %name, error = %e,
                      "compactor: configured compaction model unavailable, falling back to AUTO");
                ModelHint::default()
            }
        }
    }
}
