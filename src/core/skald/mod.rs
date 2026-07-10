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

mod accessors;
mod bundles;
mod runtime;
mod supervisor;
mod wiring;

use bundles::{Conversation, Infra, Integrations, Interaction, Media, Models, Tasks, Tools};
use runtime::Runtime;
use wiring::{spawn_background, wire};

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
        let models       = Models::build(&rt, config).await?;
        let media        = Media::build(&rt, &models).await?;
        let integrations = Integrations::build(&rt, plugins);
        let tasks        = Tasks::build(&rt, config);
        let tools        = Tools::build(&integrations, &tasks, &models);
        let interaction  = Interaction::build(&rt, &tools).await?;
        let conversation = Conversation::build(&rt, &models, &media, &tools, &integrations, &interaction, config).await?;
        let infra        = Infra::build();

        // Resolve construction cycles, then start background tasks.
        wire(&tasks, &conversation, &integrations, &interaction);
        spawn_background(&rt, &tasks, &conversation, &integrations, config);

        let skald = Arc::new(Skald {
            rt, models, media, tools, integrations, tasks, conversation, interaction, infra,
        });

        // Inject the fully-constructed instance into the plugin manager — the one
        // Arc<Skald> back-reference. start_enabled()/start_config_watcher() run later,
        // from WebFrontend::start, once the router factory is wired.
        skald.plugin_manager().set_skald(Arc::clone(&skald));

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
    }
}
