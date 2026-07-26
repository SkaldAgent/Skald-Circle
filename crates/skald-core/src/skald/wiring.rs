//! Post-construction wiring: the `OnceLock` cycle-breakers and the background-task
//! spawns, each concentrated in one readable place instead of being scattered
//! through the constructor.
//!
//! Owner-bound background loops (cron, session-cancel, ticket-listener, tic) have
//! moved per-user into `UserContextFactory::build`. What remains here are the
//! instance-wide tasks: LLM-log cleanup on the registry pool, MCP server
//! initialization, and the user-lifecycle reconciler (which needs the finished
//! `Arc<Skald>` and is therefore spawned separately, after construction).

use std::sync::Arc;

use core_api::system_bus::{RecvError, SystemEvent};
use tracing::{info, warn};

use crate::config::CoreConfig;
use crate::elicitation::ElicitationBridge;

use super::bundles::{Conversation, Integrations, Interaction, Tasks};
use super::runtime::Runtime;

/// Resolves the construction cycles (`cron ↔ session ↔ hub`, `ticket → cron`,
/// `mcp → elicitation`) via the managers' `OnceLock` setters.
///
/// These wire the **global** bundles, which are transitional and will be removed
/// once all call-sites are per-user (Phase 6). They remain constructed so any
/// not-yet-migrated accessor does not panic on a `None` OnceLock.
pub(super) fn wire(
    tasks: &Tasks,
    conversation: &Conversation,
    integrations: &Integrations,
    interaction: &Interaction,
) {
    tasks.cron.set_session(Arc::clone(&conversation.manager));
    tasks.cron.set_hub(Arc::clone(&conversation.chat_hub));
    tasks.cron.set_self_arc(Arc::clone(&tasks.cron));
    conversation.chat_hub.set_task_mgr(Arc::clone(&tasks.cron));
    integrations.mcp.set_elicitation_handler(ElicitationBridge::new(Arc::clone(&interaction.elicitation)));
    info!("ChatHub initialised");
}

/// Spawns the instance-wide background tasks.
///
/// Owner-bound loops (cron, session-cancel, ticket-listener, tic) are **not**
/// spawned here — they run per-user inside `UserContext`. Session cancellation is
/// handled directly by the API handlers (which have `AuthUser` and resolve the
/// per-user context). TIC is deferred until connectors return (§13).
pub(super) fn spawn_background(
    rt: &Runtime,
    _tasks: &Tasks,
    _conversation: &Conversation,
    integrations: &Integrations,
    config: &CoreConfig,
) {
    // LLM request-log retention/cleanup — first run 1 min after startup, then 12h.
    if let Some(cfg) = config.llm.requests_log.clone().filter(|r| r.enabled) {
        rt.supervisor.adopt_one(
            "llm-log-cleanup",
            crate::db::llm_requests::cleanup::spawn(
                Arc::clone(&rt.db),
                cfg,
                rt.shutdown_token.clone(),
            ),
        );
    }

    // MCP servers connect in the background. `initialize()` does not itself observe
    // the cancellation token, so race it against shutdown: on cancel the task exits
    // promptly (dropping the in-flight connection attempts) instead of blocking the
    // shutdown join until the deadline.
    {
        let mcp = Arc::clone(&integrations.mcp);
        let sd = rt.shutdown_token.clone();
        rt.supervisor.spawn("mcp-init", async move {
            tokio::select! {
                _ = sd.cancelled() => {}
                _ = mcp.initialize() => {}
            }
        });
    }
}

/// Spawns the **user-lifecycle reconciler** — the single subscriber that turns
/// user and membership events into container work (blueprint §6).
///
/// Producers (the Users admin page, the setup wizard, the shared-folder and
/// project membership endpoints) only announce *what changed*; none of them
/// reaches into [`ContainerManager`](crate::container::ContainerManager). That
/// is the point of routing this through the bus rather than calling the manager
/// from each handler: a future endpoint that grants membership cannot forget to
/// remount, because remounting was never its job.
///
/// Every reaction is **best-effort by contract**: the row is already committed
/// when the event fires, so a Docker hiccup is logged, never surfaced to the
/// caller — the state settles at the user's next login or at boot
/// reconciliation. Events are handled **sequentially**, which also serialises
/// concurrent `docker` operations on the same container.
///
/// Spawned after `Skald` is fully built (like `set_skald`) because
/// [`Skald::refresh_user_mounts`] is an accessor on the finished instance. The
/// back-reference is [`std::sync::Weak`], so this task never keeps `Skald` alive.
pub(super) fn spawn_user_lifecycle(skald: &Arc<super::Skald>) {
    let weak     = Arc::downgrade(skald);
    let shutdown = skald.rt.shutdown_token.clone();
    let mut rx   = skald.rt.system_bus.subscribe();

    skald.rt.supervisor.spawn("user-lifecycle", async move {
        loop {
            let event = tokio::select! {
                _ = shutdown.cancelled() => break,
                event = rx.recv() => match event {
                    Ok(e) => e,
                    // A dropped event costs a stale container until the user's next
                    // login/boot — never a lost row, since the DB write came first.
                    Err(RecvError::Lagged(n)) => {
                        warn!(n, "user-lifecycle: system_bus lagged; container state may be stale until next login/boot");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                },
            };

            let Some(skald) = weak.upgrade() else { break };
            match event {
                SystemEvent::UserCreated { user_id } => {
                    if let Err(e) = skald.container().ensure(&user_id).await {
                        warn!(user = %user_id, error = %e,
                            "user-lifecycle: failed to provision container (retried at next boot)");
                    }
                }
                SystemEvent::UserDeleted { user_id } => {
                    if let Err(e) = skald.container().remove(&user_id).await {
                        warn!(user = %user_id, error = %e,
                            "user-lifecycle: failed to remove container");
                    }
                }
                SystemEvent::UserMountsChanged { user_id } => {
                    if let Err(e) = skald.refresh_user_mounts(&user_id).await {
                        warn!(user = %user_id, error = %e,
                            "user-lifecycle: remount failed (settles at next login/boot)");
                    }
                }
                _ => {}
            }
        }
        info!("user-lifecycle: reconciler stopped");
    });
}
