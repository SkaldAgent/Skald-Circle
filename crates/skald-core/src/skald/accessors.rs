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

    /// Revokes a user's live runtime: sessions, owner-bound loops, database key.
    ///
    /// Called when a user is **deactivated or deleted**. Writing `active = 0` (or
    /// deleting the row) only stops the *next* login: `login` checks the flag, but
    /// `require_auth` maps token → id without re-reading the row, so a session minted
    /// before the change would keep working, over a pool whose key is still in RAM.
    ///
    /// The order is load-bearing:
    ///
    /// 1. **Revoke the sessions** — the moment this returns, no token authenticates
    ///    as this user.
    /// 2. **Evict the context** — cancels their cron loop, hub and per-user MCP
    ///    runtime, so nothing is left to query the pool we are about to close.
    /// 3. **Lock the database** — `close()`s the pool, which invalidates every
    ///    surviving clone and drops the DEK (§9). The user is opaque again.
    ///
    /// Synchronous by design: this is an authorization invariant, not reconciliation,
    /// so it must not ride the lossy system bus. The Docker half (stop or remove the
    /// container) *is* reconciliation and does ride it.
    ///
    /// Idempotent — a user with no live session and a locked database is a no-op.
    pub async fn revoke_user_runtime(&self, user_id: &str) {
        self.sessions().revoke_user(user_id);
        self.rt_user_contexts().evict(user_id).await;
        self.rt.users.lock(user_id).await;
    }

    /// Re-checks a live user's open sessions against their current role, degrading any
    /// security group the role no longer allows, and tells their open tabs about it.
    ///
    /// The durable half of this is in `ChatSessionManager::get_or_create_handler`,
    /// which reconciles on every load; this is the liveness half, for sessions already
    /// in RAM. Synchronous, like [`Self::revoke_user_runtime`] and for the same reason:
    /// narrowing someone's permissions is an authorization change, not reconciliation.
    ///
    /// No-op for a user who is not logged in — their next login loads through the
    /// reconcile anyway.
    pub async fn revalidate_security_groups_for_user(&self, user_id: &str) {
        let Some(ctx) = self.user_context_if_live(user_id).await else { return };
        for (source, group) in ctx.sessions.revalidate_security_groups().await {
            ctx.chat_hub.emit(core_api::events::GlobalEvent {
                source:     Some(source),
                session_id: None,
                event:      core_api::events::ServerEvent::SecurityGroupSelected { group },
            });
        }
    }

    /// [`Self::revalidate_security_groups_for_user`] for every member of a role —
    /// called when the role's own group set changes, which can narrow many users at
    /// once. Members who are not logged in need nothing.
    pub async fn revalidate_security_groups_for_role(&self, role_id: &str) {
        let users = match crate::db::users::list(self.db()).await {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(role = %role_id, error = %e,
                    "cannot list users to revalidate security groups");
                return;
            }
        };
        for user in users.into_iter().filter(|u| u.role_id == role_id) {
            self.revalidate_security_groups_for_user(&user.id).await;
        }
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

    /// Refresh every live user's global-connector access set in place — call after an
    /// admin enables/deletes a global connector or changes who may use it, so running
    /// sessions see it without a restart (the §7 MCP twin of the §6 fs remount). The
    /// global runtime itself is already updated by the caller (`start_server` /
    /// `stop_server`); this only re-snapshots each user's access filter. Best-effort:
    /// a locked (not-live) user has no snapshot to refresh — their next login rebuilds
    /// it from the now-current tables.
    pub async fn refresh_global_mcp_access(&self) {
        for ctx in self.rt_user_contexts().all_live().await {
            if let Err(e) = ctx.refresh_global_access().await {
                tracing::warn!(user = %ctx.user_id, error = %e, "failed to refresh global MCP access");
            }
        }
    }

    /// Pushes a marketplace **reinstall** into every live copy of the connector so
    /// active sessions pick up the new metadata (`llm_short_description`) and code
    /// without a re-login — the reinstall counterpart of the §6/§7 remount helpers.
    /// The reinstall has already rewritten `mcp_catalog`; this reconnects what runs:
    ///
    /// - **Global runtime**: for each *enabled* `mcp_global_servers` row snapshotting
    ///   this catalog entry, re-snapshot its `description` from the catalog and restart
    ///   it, so the running server's in-RAM description (and code) catches up.
    /// - **Per-user runtimes**: for each live user who has this connector *startable*,
    ///   re-copy its files/deps into the container (`prepare_local_connector` — a hash
    ///   no-op when the source is unchanged) and restart that one server. The rebuilt
    ///   spec now carries the fresh catalog description (see `user_row_spec_resolved`).
    ///
    /// Best-effort: the catalog write already committed, so a Docker/MCP hiccup here
    /// must not fail the reinstall — anything not refreshed settles at the user's next
    /// login. A fresh install (nothing live yet) is a cheap no-op: no row matches.
    pub async fn refresh_connector_after_reinstall(&self, catalog_name: &str) {
        // The metadata the reinstall just wrote — the source of truth to push out.
        let entry = match crate::db::mcp_catalog::get_by_name(self.db(), catalog_name).await {
            Ok(Some(e)) => e,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(connector = %catalog_name, error = %e, "reinstall refresh: catalog lookup failed");
                return;
            }
        };

        // 1. Global runtime.
        if let Ok(globals) = crate::db::mcp_global_servers::all_enabled(self.db()).await {
            for g in globals.iter().filter(|g| g.catalog_name.as_deref() == Some(catalog_name)) {
                if let Err(e) = crate::db::mcp_global_servers::set_description(self.db(), g.id, entry.description.as_deref()).await {
                    tracing::warn!(connector = %catalog_name, error = %e, "reinstall refresh: failed to update global description");
                    continue;
                }
                match crate::db::mcp_global_servers::get(self.db(), g.id).await {
                    Ok(Some(row)) => {
                        let spec = crate::mcp::global_row_spec(&row);
                        if let Err(e) = self.mcp().start_server(spec).await {
                            tracing::warn!(connector = %catalog_name, error = %e, "reinstall refresh: failed to restart global server");
                        }
                    }
                    _ => tracing::warn!(connector = %catalog_name, "reinstall refresh: global row vanished before restart"),
                }
            }
        }

        // 2. Per-user runtimes — restart this one connector for each live user who runs it.
        for ctx in self.rt_user_contexts().all_live().await {
            let rows = crate::db::mcp_user_servers::all_startable(&ctx.pool).await.unwrap_or_default();
            let Some(row) = rows.into_iter().find(|r| r.catalog_name.as_deref() == Some(catalog_name)) else {
                continue;
            };
            let container = crate::container::container_name(&ctx.user_id);
            crate::mcp::prepare_local_connector(self.db(), &ctx.user_id, &container, &row).await;
            let spec = crate::mcp::user_row_spec_resolved(&row, &container, self.db()).await;
            if let Err(e) = ctx.user_mcp.start_server(spec).await {
                tracing::warn!(user = %ctx.user_id, connector = %catalog_name, error = %e, "reinstall refresh: failed to restart per-user connector");
            }
        }
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
