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
use core_api::user_channel::UserChannelApi;

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

    /// The caller's per-user owner-bound runtime (chat/hub/cron/interaction),
    /// built lazily on first use. `None` when the user's database is still locked
    /// (not logged in). The pool is the unlock token (§9); a present pool means an
    /// unlocked database, so a context can be built for it.
    pub async fn user_context(&self, user_id: &str) -> Option<Arc<super::UserContext>> {
        let pool = self.rt.users.pool_of(user_id)?;
        self.rt_user_contexts().resolve(user_id, pool).await.ok()
    }

    fn rt_user_contexts(&self) -> &super::user_context::UserContextRegistry { &self.user_contexts }

    /// The user's runtime context IF it is already live (built), **without**
    /// building one — used to refresh a logged-in user in place. A user who never
    /// logged in has no snapshot to refresh; their next login builds a fresh one.
    pub async fn user_context_if_live(&self, user_id: &str) -> Option<Arc<super::UserContext>> {
        self.rt_user_contexts().peek(user_id).await
    }

    /// Applies a shared-folder membership change to a user (blueprint §6 remount).
    ///
    /// A container's bind mounts are fixed at `docker create` time, so the mount set
    /// changes only by recreating the container — done here with a graceful stop
    /// first ([`ContainerManager::recreate`](crate::container::ContainerManager::recreate)).
    /// If the user is **live**, the two snapshot-bound pieces are then refreshed in
    /// place against the fresh container: their filesystem view (which governs both
    /// the host-side fs-tools and `execute_cmd` path routing) and their per-user MCP
    /// runtime (whose `docker exec` children died with the old container). A user
    /// with no live context needs only the recreate — their next login builds a
    /// context that already reflects the change.
    ///
    /// Best-effort by contract: the membership row is already committed, so a Docker
    /// hiccup must not fail the caller; the state settles at the next login/boot.
    ///
    /// Covers both shared-folder and project membership changes — both feed
    /// `build_user_fs`, so a recreate reflows either mount set.
    pub async fn refresh_user_mounts(&self, user_id: &str) -> anyhow::Result<()> {
        // New mount topology (graceful stop → remove → recreate from current rows).
        self.container().recreate(user_id).await?;

        let Some(ctx) = self.user_context_if_live(user_id).await else {
            return Ok(()); // not logged in — next login builds a fresh context
        };

        // fs view: swap the shared cell so every live session picks it up next call.
        let new_fs = crate::container::build_user_fs(self.db(), user_id).await?;
        ctx.sessions.refresh_fs(new_fs);

        // per-user MCP: the old container's `docker exec` children are gone. Stop the
        // stale handles, then reconnect the activated connectors against the fresh
        // container (same deterministic name).
        ctx.user_mcp.stop_all();
        let rows = crate::db::mcp_user_servers::all_startable(&ctx.pool).await.unwrap_or_default();
        if !rows.is_empty() {
            let container = crate::container::container_name(user_id);
            let mut specs = Vec::with_capacity(rows.len());
            for r in &rows {
                // The home mount (and its `node_modules`/`.pydeps`) survives a
                // recreate, so this is normally a hash-match no-op; it still covers
                // the case where the source changed while the user was logged in.
                crate::mcp::prepare_local_connector(self.db(), user_id, &container, r).await;
                specs.push(crate::mcp::user_row_spec_resolved(r, &container, self.db()).await);
            }
            ctx.user_mcp.connect_all(specs, false).await;
        }
        Ok(())
    }
    pub fn sessions(&self) -> &Arc<crate::auth::SessionStore> { &self.rt.sessions }
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

// ── UserChannelApi ────────────────────────────────────────────────────────────

use super::user_context::UserContextHandle;

#[async_trait::async_trait]
impl UserChannelApi for Skald {
    async fn resolve_user(&self, user_id: &str) -> Option<std::sync::Arc<dyn core_api::user_channel::UserChannelHandle>> {
        let ctx = self.user_context(user_id).await?;
        Some(std::sync::Arc::new(UserContextHandle::new(ctx)))
    }

    async fn plugin_access(&self, plugin_id: &str, user_id: &str) -> bool {
        // Admin short-circuit + grant lookup live in `db::plugin_access`; a
        // lookup error fails closed.
        crate::db::plugin_access::effective_access(self.db(), plugin_id, user_id)
            .await
            .unwrap_or(false)
    }

    async fn is_admin(&self, user_id: &str) -> bool {
        // Built-in admin role; an unknown user or a lookup error fails closed.
        sqlx::query_as::<_, (String,)>("SELECT role_id FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_optional(self.db().as_ref())
            .await
            .ok()
            .flatten()
            .map(|(r,)| r == crate::db::roles::ADMIN_ROLE_ID)
            .unwrap_or(false)
    }

    async fn user_for_session(&self, token: &str) -> Option<String> {
        self.sessions().user_of(token)
    }
}
