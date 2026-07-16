//! Domain bundles: the managers, grouped by cohesion, that make up `Skald`.
//!
//! Each bundle owns a `build()` that constructs its managers (plus their startup
//! logging and non-fatal `seed_*` calls) from the shared [`Runtime`] and whatever
//! sibling bundles it depends on at construction time. Cross-bundle *cycles* are
//! not expressed here — they are resolved by the managers' `OnceLock` setters,
//! called in one place by [`super::wiring::wire`]. Bundle structs never hold
//! references to each other.

use std::sync::Arc;

use anyhow::Result;
use tracing::{debug, info, warn};

use core_api::remote::RemoteAccess;

use crate::approval::ApprovalManager;
use crate::chat_hub::ChatHub;
use crate::clarification::ClarificationManager;
use crate::command::LlmCommandManager;
use crate::compactor::ContextCompactor;
use crate::config::{CoreConfig, DatetimeConfig};
use crate::cron::TaskManager;
use crate::elicitation::ElicitationManager;
use crate::image_generate::ImageGeneratorManager;
use crate::inbox::Inbox;
use crate::latex::LatexCompiler;
use crate::llm::LlmManager;
use crate::location::LocationManager;
use crate::mcp::McpManager;
use crate::memory::MemoryManager;
use crate::plugin::PluginManager;
use crate::projects::tickets::ProjectTicketManager;
use crate::projects::ProjectManager;
use crate::provider::ProviderRegistry;
use crate::run_context::RunContextManager;
use crate::secrets::SecretsStore;
use crate::session::handler::{DEFAULT_MAX_PARALLEL_SUBAGENTS, DEFAULT_MAX_TOOL_ROUNDS};
use crate::session::manager::ChatSessionManager;
use crate::tic::TicManager;
use crate::tool_catalog::ToolCatalog;
use crate::tool_discovery::ToolDiscovery;
use crate::tools::ToolRegistry;
use crate::transcribe::TranscribeManager;
use crate::tts::TtsManager;

use tokio::sync::RwLock;

use core_api::plugin::Plugin;

use super::runtime::Runtime;

// ── Models: LLM/provider stack ──────────────────────────────────────────────

pub(super) struct Models {
    pub(super) provider_registry: Arc<ProviderRegistry>,
    pub(super) llm_manager:       Arc<LlmManager>,
    pub(super) secrets:           Arc<SecretsStore>,
    pub(super) memory_manager:    Arc<MemoryManager>,
}

impl Models {
    pub(super) async fn build(rt: &Runtime, config: &CoreConfig) -> Result<Self> {
        let mut provider_registry = ProviderRegistry::new(Arc::clone(&rt.system_bus));
        provider_registry.register_builtin(crate::llm::providers::openai::OpenAiProvider);
        provider_registry.register_builtin(crate::llm::providers::anthropic::AnthropicProvider::new());
        provider_registry.register_builtin(crate::llm::providers::openrouter::OpenRouterProvider::new());
        provider_registry.register_builtin(crate::llm::providers::ollama::OllamaProvider::new());
        provider_registry.register_builtin(crate::llm::providers::lm_studio::LmStudioProvider::new());
        provider_registry.register_builtin(crate::llm::providers::deepseek::DeepSeekProvider::new());
        provider_registry.register_builtin(crate::llm::providers::zai::ZaiProvider::new());
        let provider_registry = Arc::new(provider_registry);
        info!("provider registry ready ({} built-in providers)", provider_registry.all().len());

        let log_enabled = config.llm.requests_log.as_ref().is_some_and(|r| r.enabled);
        let llm_manager = LlmManager::new(Arc::clone(&rt.db), Arc::clone(&provider_registry), log_enabled).await?;
        let client_count = llm_manager.client_names().await.len().saturating_sub(1);
        let default_client = llm_manager.default_name().await;
        info!(clients = client_count, default = %default_client, "LLM clients loaded");

        let secrets = SecretsStore::new(Arc::clone(&rt.db));
        info!("secrets store ready");

        let memory_manager = Arc::new(MemoryManager::new());
        info!("memory manager ready");

        Ok(Models { provider_registry, llm_manager, secrets, memory_manager })
    }
}

// ── Media: transcription / TTS / image generation ───────────────────────────

pub(super) struct Media {
    pub(super) image_generator_manager: Arc<ImageGeneratorManager>,
    pub(super) transcribe_manager:      Arc<TranscribeManager>,
    pub(super) tts_manager:             Arc<TtsManager>,
}

impl Media {
    pub(super) async fn build(rt: &Runtime, models: &Models) -> Result<Self> {
        let image_generator_manager = ImageGeneratorManager::new(
            Arc::clone(&rt.db),
            Arc::clone(&models.provider_registry),
            "data",
        ).await?;
        // Evaluate the await outside the `info!` macro: leaving the temporary
        // `tracing::Value` from the field expression alive across the await
        // makes the surrounding future non-Send, which Tauri's runtime rejects.
        let image_generator_models = image_generator_manager.list_models_info().await.len();
        info!(
            db_backed = image_generator_models,
            "image generator manager ready",
        );

        let transcribe_manager = TranscribeManager::new(
            Arc::clone(&rt.db),
            Arc::clone(&models.provider_registry),
            Arc::clone(&rt.system_bus),
            rt.shutdown_token.clone(),
        ).await?;
        let transcribe_models = transcribe_manager.list_models_info().await.len();
        info!(
            db_backed = transcribe_models,
            "transcribe manager ready",
        );

        let tts_manager = TtsManager::new(
            Arc::clone(&rt.db),
            Arc::clone(&models.provider_registry),
            Arc::clone(&rt.system_bus),
            rt.shutdown_token.clone(),
        ).await?;
        let tts_models = tts_manager.list_models_info().await.len();
        info!(
            db_backed = tts_models,
            "tts manager ready",
        );

        Ok(Media { image_generator_manager, transcribe_manager, tts_manager })
    }
}

// ── Integrations: MCP + plugins ─────────────────────────────────────────────

pub(super) struct Integrations {
    pub(super) mcp:            Arc<McpManager>,
    pub(super) plugin_manager: Arc<PluginManager>,
}

impl Integrations {
    /// Builds the MCP manager (its `initialize()` is deferred to `spawn_background`,
    /// after the elicitation handler is wired) and the plugin manager (plugins are
    /// injected by `main.rs`; `start_enabled()` runs later, from `WebFrontend`).
    pub(super) fn build(rt: &Runtime, plugins: Vec<Arc<dyn Plugin>>) -> Self {
        let mcp = Arc::new(McpManager::new(Arc::clone(&rt.db), rt.shutdown_token.clone(), "data"));

        let mut plugin_manager = PluginManager::new(Arc::clone(&rt.db));
        for plugin in plugins {
            plugin_manager.register_arc(plugin);
        }
        info!("plugins registered");
        let plugin_manager = Arc::new(plugin_manager);

        Integrations { mcp, plugin_manager }
    }
}

// ── Tasks: cron + projects/tickets ──────────────────────────────────────────

pub(super) struct Tasks {
    pub(super) cron:           Arc<TaskManager>,
    pub(super) projects:       Arc<ProjectManager>,
    pub(super) ticket_manager: Arc<ProjectTicketManager>,
}

impl Tasks {
    /// Built before `Tools` so cron tools can capture the `TaskManager`.
    pub(super) fn build(rt: &Runtime, config: &CoreConfig) -> Self {
        let cron_tz = config.timezone.as_deref().and_then(|s| {
            match s.parse::<chrono_tz::Tz>() {
                Ok(tz)  => { info!("timezone: using {s}"); Some(tz) }
                Err(_)  => { warn!("timezone: unknown value '{s}', falling back to local time"); None }
            }
        });
        let cron = TaskManager::new(Arc::clone(&rt.db), cron_tz, Arc::clone(&rt.system_bus));

        let ticket_manager = ProjectTicketManager::new(Arc::clone(&rt.db));
        let projects       = Arc::new(ProjectManager::new(Arc::clone(&rt.db)));
        info!("project manager ready");

        Tasks { cron, projects, ticket_manager }
    }
}

// ── Tools: registry + catalog + slash commands ──────────────────────────────

pub(super) struct Tools {
    pub(super) tools:           Arc<ToolRegistry>,
    pub(super) catalog:         ToolCatalog,
    pub(super) command_manager: Arc<LlmCommandManager>,
}

impl Tools {
    /// Captures sibling managers (mcp, plugins, cron, secrets) into the tool
    /// registry. `execute_task` is deliberately NOT registered here — it is injected
    /// per interactive session by `ChatHub::send_message`.
    pub(super) fn build(rt: &Runtime, integrations: &Integrations, tasks: &Tasks, models: &Models) -> Self {
        let mut tool_registry = ToolRegistry::new();
        crate::tools::fs::register_all(&mut tool_registry, Arc::clone(&rt.db));
        tool_registry.register(crate::tools::ast_outline::AstOutline::new());
        tool_registry.register(crate::tools::exec::ExecuteCmd);
        tool_registry.register(crate::tools::read_notification::ReadNotification);
        tool_registry.register(crate::tools::restart::Restart);
        // Unified listing / toggling across plugins, cron (+ agents for list). MCP
        // is no longer agent-managed (blueprint §14): connectors are curated by the
        // admin and activated by the user via the Connectors UI/API, not tools.
        tool_registry.register(crate::tools::list_items::ListItems::new(
            Arc::clone(&integrations.plugin_manager), Arc::clone(&tasks.cron)));
        tool_registry.register(crate::tools::toggle_item::ToggleItem::new(
            Arc::clone(&integrations.plugin_manager), Arc::clone(&tasks.cron)));
        tool_registry.register(crate::tools::cron_jobs::DeleteCronJob);
        tool_registry.register(crate::tools::set_secret::SetSecret(Arc::clone(&models.secrets)));
        tool_registry.register(crate::tools::list_secrets::ListSecrets(Arc::clone(&models.secrets)));
        tool_registry.register(crate::tools::configure_plugin::ConfigurePlugin(Arc::clone(&integrations.plugin_manager)));

        // Tools contributed by plugins (plugin.md §11), via `Plugin::tools()`.
        // The core never names a plugin crate: each one hands over whatever tools
        // it wants, bound to its own handle. They are built before the plugins'
        // runloops start, so they must tolerate being called while stopped.
        for plugin in integrations.plugin_manager.all() {
            let id = plugin.id().to_string();
            let tools = Arc::clone(plugin).tools();
            if tools.is_empty() {
                continue;
            }
            let n = tools.len();
            for tool in tools {
                tool_registry.register_arc(tool);
            }
            info!(plugin = %id, count = n, "plugin tools registered");
        }
        debug!("tool registry built");

        let tools = Arc::new(tool_registry);
        let catalog = ToolCatalog::new(Arc::clone(&tools), Arc::clone(&integrations.mcp));
        let command_manager = Arc::new(LlmCommandManager::new());

        Tools { tools, catalog, command_manager }
    }
}

// ── Interaction: approval + inbox + clarification + elicitation ─────────────

pub(super) struct Interaction {
    pub(super) approval:      Arc<ApprovalManager>,
    pub(super) inbox:         Inbox,
    pub(super) clarification: Arc<ClarificationManager>,
    pub(super) elicitation:   Arc<ElicitationManager>,
}

impl Interaction {
    pub(super) async fn build(rt: &Runtime, tools: &Tools) -> Result<Self> {
        let approval = Arc::new(ApprovalManager::new(Arc::clone(&rt.db), rt.global_tx.clone()));
        if let Err(e) = approval.seed_defaults().await {
            warn!(error = %e, "failed to seed default approval rules (non-fatal)");
        }
        if let Err(e) = approval.migrate_legacy_fs_rules().await {
            warn!(error = %e, "failed to migrate legacy filesystem rules (non-fatal)");
        }
        if let Err(e) = approval.seed_fs_path_rules().await {
            warn!(error = %e, "failed to seed File System path rules (non-fatal)");
        }
        if let Err(e) = approval.seed_default_catch_all().await {
            warn!(error = %e, "failed to seed default catch-all rule (non-fatal)");
        }
        info!("approval manager ready");

        let clarification = ClarificationManager::new(rt.global_tx.clone());
        let elicitation = ElicitationManager::new(rt.global_tx.clone());

        let inbox = Inbox::new(
            Arc::clone(&approval),
            Arc::clone(&clarification),
            Arc::clone(&elicitation),
            Arc::clone(&tools.tools),
        );

        Ok(Interaction { approval, inbox, clarification, elicitation })
    }
}

// ── Conversation: session manager + chat hub + run context + TIC ────────────

pub(super) struct Conversation {
    pub(super) manager:             Arc<ChatSessionManager>,
    pub(super) chat_hub:            Arc<ChatHub>,
    pub(super) run_context_manager: Arc<RunContextManager>,
    /// TIC lives here (rather than in `Tasks`) because it is constructed from and
    /// drives the conversation stack (session manager + chat hub + run context);
    /// this keeps every bundle a single-shot `build()` with no two-phase init.
    pub(super) tic_manager:         Arc<TicManager>,
}

impl Conversation {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn build(
        rt: &Runtime,
        models: &Models,
        media: &Media,
        tools: &Tools,
        integrations: &Integrations,
        interaction: &Interaction,
        config: &CoreConfig,
    ) -> Result<Self> {
        let run_context_manager =
            Arc::new(RunContextManager::new(Arc::clone(&rt.db), Arc::clone(&interaction.approval)));
        if let Err(e) = run_context_manager.seed_defaults().await {
            warn!(error = %e, "failed to seed default permission group (non-fatal)");
        }
        info!("run_context manager ready");

        let compactor = config.llm.compaction.as_ref().map(|cfg| {
            info!(
                threshold_tokens = cfg.threshold_tokens,
                keep_recent      = cfg.keep_recent,
                ?cfg.strength,
                "context compactor enabled"
            );
            Arc::new(ContextCompactor::new(
                cfg.clone(),
                Arc::clone(&models.llm_manager),
                Arc::clone(&rt.event_bus),
            ))
        });
        if compactor.is_none() {
            info!("context compactor disabled (no compaction config)");
        }

        // The ownerless manager is inert (no loops, no consumers — see §19): it takes
        // a placeholder UserFs purely to satisfy the type, never used to resolve a path.
        let ownerless_fs = Arc::new(core_api::user_fs::UserFs::new(
            String::new(),
            std::path::PathBuf::from("homes"),
            "skald-ownerless",
            std::path::PathBuf::from("/root"),
            Vec::new(),
        ));

        let manager = Arc::new(ChatSessionManager::new(
            Arc::clone(&rt.db),
            Arc::clone(&rt.db), // shared pool == system.db (this is the ownerless manager)
            String::new(),
            ownerless_fs,
            Arc::clone(&models.llm_manager),
            config.llm.max_history_messages,
            config.llm.max_tool_rounds.unwrap_or(DEFAULT_MAX_TOOL_ROUNDS),
            config.llm.max_parallel_subagents.unwrap_or(DEFAULT_MAX_PARALLEL_SUBAGENTS),
            config.llm.max_tool_result_chars,
            DatetimeConfig { timezone: config.timezone.clone(), ..config.llm.datetime },
            Arc::clone(&tools.tools),
            // Inert ownerless bundle (§19): the global runtime as a provider,
            // unfiltered — never actually exercised (no loops, no consumers).
            Arc::clone(&integrations.mcp) as Arc<dyn crate::mcp::McpProvider>,
            Arc::clone(&interaction.approval),
            Arc::clone(&interaction.clarification),
            Arc::clone(&rt.event_bus),
            Arc::clone(&models.memory_manager),
            Arc::clone(&media.image_generator_manager),
            compactor,
            Arc::clone(&run_context_manager),
            Arc::new(ToolDiscovery::new(Arc::clone(&rt.db))),
        ));

        let chat_hub = ChatHub::new(
            Arc::clone(&rt.db),
            Arc::clone(&manager),
            Arc::clone(&interaction.approval),
            rt.global_tx.clone(),
            rt.shutdown_token.clone(),
        );
        chat_hub.register("web").await;
        chat_hub.register("talk").await;

        let tic_manager = TicManager::new(
            Arc::clone(&rt.db),
            Arc::clone(&manager),
            Arc::clone(&chat_hub),
            config.tic.clone(),
            Arc::clone(&rt.config),
            Arc::clone(&run_context_manager),
            Arc::clone(&rt.system_bus),
        );

        Ok(Conversation { manager, chat_hub, run_context_manager, tic_manager })
    }
}

// ── Infra: leftover singletons ──────────────────────────────────────────────

pub(super) struct Infra {
    pub(super) latex_compiler:   LatexCompiler,
    pub(super) location_manager: Arc<LocationManager>,
    pub(super) remote:           Arc<RwLock<Option<Arc<dyn RemoteAccess>>>>,
}

impl Infra {
    pub(super) fn build() -> Self {
        Infra {
            latex_compiler:   LatexCompiler::new(),
            location_manager: Arc::new(LocationManager::new()),
            remote:           Arc::new(RwLock::new(None)),
        }
    }
}
