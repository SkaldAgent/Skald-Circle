//! The logical API surface of `Skald`: one accessor per manager, named after the
//! historical field, delegating into the domain bundle that now owns it. This is
//! the intentional surface consumers (frontend handlers, plugin context) use — the
//! bundles themselves stay internal.
//!
//! Now that the core is its own crate, this is a real boundary rather than a
//! convention: everything here is `pub` because the `skald` binary lives outside,
//! and everything not here is unreachable from it. Promote the block to a
//! `SkaldApi` trait if the shells ever need to mock it.

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use core_api::remote::RemoteAccess;
use core_api::system_bus::SystemEventBus;

use crate::approval::ApprovalManager;
use crate::chat_event_bus::ChatEventBus;
use crate::chat_hub::ChatHub;
use crate::clarification::ClarificationManager;
use crate::command::LlmCommandManager;
use crate::config_store::GlobalConfigManager;
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
use crate::session::manager::ChatSessionManager;
use crate::tic::TicManager;
use crate::tool_catalog::ToolCatalog;
use crate::tools::ToolRegistry;
use crate::transcribe::TranscribeManager;
use crate::tts::TtsManager;
use crate::users::UserManager;

use super::Skald;

impl Skald {
    // Runtime / cross-cutting
    pub fn db(&self) -> &Arc<SqlitePool> { &self.rt.db }
    pub fn users(&self) -> &Arc<UserManager> { &self.rt.users }
    pub fn config(&self) -> &Arc<GlobalConfigManager> { &self.rt.config }
    pub fn config_properties(&self) -> &[core_api::ConfigSet] { &self.rt.config_properties }
    pub fn system_bus(&self) -> &Arc<SystemEventBus> { &self.rt.system_bus }
    pub fn event_bus(&self) -> &Arc<ChatEventBus> { &self.rt.event_bus }
    pub fn shutdown_token(&self) -> &CancellationToken { &self.rt.shutdown_token }

    // Models
    pub fn provider_registry(&self) -> &Arc<ProviderRegistry> { &self.models.provider_registry }
    pub fn llm_manager(&self) -> &Arc<LlmManager> { &self.models.llm_manager }
    pub fn secrets(&self) -> &Arc<SecretsStore> { &self.models.secrets }
    pub fn memory_manager(&self) -> &Arc<MemoryManager> { &self.models.memory_manager }

    // Media
    pub fn image_generator_manager(&self) -> &Arc<ImageGeneratorManager> { &self.media.image_generator_manager }
    pub fn transcribe_manager(&self) -> &Arc<TranscribeManager> { &self.media.transcribe_manager }
    pub fn tts_manager(&self) -> &Arc<TtsManager> { &self.media.tts_manager }

    // Tools
    pub fn tools(&self) -> &Arc<ToolRegistry> { &self.tools.tools }
    pub fn catalog(&self) -> &ToolCatalog { &self.tools.catalog }
    pub fn command_manager(&self) -> &Arc<LlmCommandManager> { &self.tools.command_manager }

    // Integrations
    pub fn mcp(&self) -> &Arc<McpManager> { &self.integrations.mcp }
    pub fn plugin_manager(&self) -> &Arc<PluginManager> { &self.integrations.plugin_manager }

    // Tasks
    pub fn cron(&self) -> &Arc<TaskManager> { &self.tasks.cron }
    pub fn projects(&self) -> &Arc<ProjectManager> { &self.tasks.projects }
    pub fn ticket_manager(&self) -> &Arc<ProjectTicketManager> { &self.tasks.ticket_manager }

    // Conversation
    pub fn manager(&self) -> &Arc<ChatSessionManager> { &self.conversation.manager }
    pub fn chat_hub(&self) -> &Arc<ChatHub> { &self.conversation.chat_hub }
    pub fn run_context_manager(&self) -> &Arc<RunContextManager> { &self.conversation.run_context_manager }
    pub fn tic_manager(&self) -> &Arc<TicManager> { &self.conversation.tic_manager }

    // Interaction
    pub fn approval(&self) -> &Arc<ApprovalManager> { &self.interaction.approval }
    pub fn inbox(&self) -> &Inbox { &self.interaction.inbox }
    pub fn clarification(&self) -> &Arc<ClarificationManager> { &self.interaction.clarification }
    pub fn elicitation(&self) -> &Arc<ElicitationManager> { &self.interaction.elicitation }

    // Infra
    pub fn latex_compiler(&self) -> &LatexCompiler { &self.infra.latex_compiler }
    pub fn location_manager(&self) -> &Arc<LocationManager> { &self.infra.location_manager }
    pub fn remote(&self) -> &Arc<RwLock<Option<Arc<dyn RemoteAccess>>>> { &self.infra.remote }
}
