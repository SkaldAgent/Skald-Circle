//! `Skald` — the headless application core.
//!
//! `Skald` owns every manager but is no longer a God Object: the ~30 managers are
//! grouped into a cross-cutting [`Runtime`] context plus eight cohesive domain
//! bundles (see [`bundles`]). Construction is a staged composition root (each bundle
//! has its own `build()`); the construction cycles are resolved in one place by
//! [`wiring::wire`]; every background task is registered with a [`TaskSupervisor`]
//! so shutdown joins them uniformly. The frontend and plugin context consume `Skald`
//! only through the accessor methods in [`accessors`], never its fields — that
//! accessor surface is the logical boundary a future `skald-core` crate would keep.

use std::sync::Arc;

use anyhow::Result;
use sqlx::SqlitePool;
use tracing::info;

use core_api::plugin::Plugin;

use super::config::CoreConfig;
use crate::container::ContainerManager;

mod accessors;
mod bundles;
mod runtime;
mod supervisor;
mod user_context;
mod wiring;

use bundles::{Conversation, Infra, Integrations, Interaction, Media, Models, Tasks, Tools};
use runtime::Runtime;
use user_context::{UserContextFactory, UserContextRegistry};
pub use user_context::UserContext;
use wiring::{spawn_background, spawn_system_agents, spawn_user_lifecycle, wire};

pub struct Skald {
    rt:           Runtime,
    models:       Models,
    media:        Media,
    tools:        Tools,
    integrations: Integrations,
    tasks:        Tasks,
    conversation: Conversation,
    interaction:  Interaction,
    infra:        Infra,
    /// Per-user Docker containers (blueprint §6): the execution sandbox. Docker is a
    /// hard requirement — `new()` fails if the daemon is unreachable.
    container:    ContainerManager,
    /// The background agents this instance runs (blueprint §13), held here rather
    /// than inside the scheduler because they now have two starters: the timer and
    /// the "Run now" button. One list, one in-flight guard.
    system_agents: Arc<crate::system_agents::SystemAgents>,
    /// Per-user owner-bound runtimes (chat/hub/cron/interaction), built lazily on
    /// first use after a user's pool is unlocked. The global bundles above still
    /// serve deferred subsystems and the not-yet-migrated call sites.
    user_contexts: UserContextRegistry,
}

impl Skald {
    pub async fn new(pool: Arc<SqlitePool>, config: &CoreConfig, plugins: Vec<Arc<dyn Plugin>>) -> Result<Arc<Self>> {
        let discovered = super::agents::discover()?;
        info!(
            count = discovered.len(),
            agents = discovered.iter().map(|a| a.id.as_str()).collect::<Vec<_>>().join(", "),
            "agents discovered"
        );

        // ── Composition root: build the runtime context, then each domain bundle
        // in dependency order. `Tasks` precedes `Tools` (tools capture cron);
        // `Interaction` and `Conversation` come last (they need the tool registry
        // and each other's managers).
        let rt           = Runtime::bootstrap(pool);

        // Docker is REQUIRED (blueprint §6): fail fast, before the heavy managers,
        // if the daemon is unreachable — the shell then exits with this error.
        let container = ContainerManager::new(Arc::clone(&rt.db));
        container.check_docker().await?;

        let models       = Models::build(&rt, config).await?;
        let media        = Media::build(&rt, &models).await?;
        let integrations = Integrations::build(&rt, plugins);
        let tasks        = Tasks::build(&rt, config);
        let tools        = Tools::build(&rt, &integrations, &tasks, &models);
        let interaction  = Interaction::build(&rt, &tools).await?;
        let conversation = Conversation::build(&rt, &models, &media, &tools, &integrations, &interaction, config).await?;
        let infra        = Infra::build();

        // Built here rather than inside the scheduler: the "Run now" button starts
        // the same agents through the same in-flight guard (blueprint §13).
        let system_agents = crate::system_agents::SystemAgents::new(
            config.event_triage.clone(),
            Arc::clone(&rt.config),
            Arc::clone(&rt.db),
            Arc::clone(&rt.system_bus),
        );

        // Resolve construction cycles, then start background tasks.
        wire(&tasks, &conversation, &integrations, &interaction);
        spawn_background(&rt, &tasks, &conversation, &integrations, config);

        // Per-user context factory: captures the global capability managers, so a
        // per-user chat/hub/cron/interaction stack can be stamped out on demand.
        let user_contexts = UserContextRegistry::new(UserContextFactory::new(
            &rt, &models, &media, &tools, &integrations, &conversation, &container, config,
        ));

        // Build the runtime image and reconcile a container for every active user.
        // A failed image build is fatal (nothing can run); a single container that
        // won't start is logged, not fatal.
        container.reconcile_all().await?;

        let skald = Arc::new(Skald {
            rt, models, media, tools, integrations, tasks, conversation, interaction, infra,
            container,
            system_agents,
            user_contexts,
        });

        // Inject the fully-constructed instance into the plugin manager — the one
        // Arc<Skald> back-reference. start_enabled()/start_config_watcher() run later,
        // from WebFrontend::start, once the router factory is wired.
        skald.plugin_manager().set_skald(Arc::clone(&skald));

        // Same reason: the reconciler reacts through `Skald`'s own accessors, so it
        // can only be spawned once the instance exists (blueprint §6).
        spawn_user_lifecycle(&skald);

        // Likewise the system-agent scheduler: it resolves a per-user runtime for
        // each user it runs an agent for (blueprint §13).
        spawn_system_agents(&skald);

        Ok(skald)
    }

    pub fn subscribe_chat_events(&self) -> tokio::sync::broadcast::Receiver<core_api::bus::BusEvent> {
        self.rt.event_bus.subscribe()
    }

    pub fn subscribe_system_events(&self) -> tokio::sync::broadcast::Receiver<core_api::system_bus::SystemEvent> {
        self.rt.system_bus.subscribe()
    }

    pub async fn shutdown(self: Arc<Self>) {
        self.rt.shutdown_token.cancel();
        self.rt.supervisor.join_all(tokio::time::Duration::from_secs(10)).await;
        self.integrations.plugin_manager.stop_all().await;
        // Stop the per-user containers (best-effort).
        if let Err(e) = self.container.stop_all().await {
            tracing::warn!(error = %e, "failed to stop user containers");
        }
        // Last: every user key leaves RAM. A restarted box is opaque again until
        // each user unlocks their own database (§9).
        self.rt.users.lock_all().await;
    }

    /// The container manager. The API layer no longer calls it: user provisioning
    /// and teardown are driven by the lifecycle reconciler reacting to
    /// `SystemEvent::User*` (see `wiring::spawn_user_lifecycle`).
    pub fn container(&self) -> ContainerManager {
        self.container.clone()
    }
}
