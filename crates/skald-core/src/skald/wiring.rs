//! Post-construction wiring: the `OnceLock` cycle-breakers and the background-task
//! spawns, each concentrated in one readable place instead of being scattered
//! through the constructor.
//!
//! Owner-bound background loops (cron, session-cancel, ticket-listener) have
//! moved per-user into `UserContextFactory::build`. What remains here are the
//! instance-wide tasks: LLM-log cleanup on the registry pool, MCP server
//! initialization, and the two that need the finished `Arc<Skald>` and are
//! therefore spawned separately, after construction — the user-lifecycle
//! reconciler and the system-agent scheduler.

use std::sync::Arc;
use std::time::Duration;

use core_api::system_bus::{RecvError, SystemEvent};
use tracing::{info, warn};

use crate::config::{CoreConfig, TicConfig};
use crate::elicitation::ElicitationBridge;
use crate::tic::{TicManager, TIC_INTERVAL_MINUTES_KEY};

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
/// Owner-bound loops (cron, session-cancel, ticket-listener) are **not** spawned
/// here — they run per-user inside `UserContext`. Session cancellation is handled
/// directly by the API handlers (which have `AuthUser` and resolve the per-user
/// context). The system-agent scheduler needs the finished instance and lives in
/// [`spawn_system_agents`].
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
                // Match boot reconciliation, which keeps a container only for active
                // users. The user's live runtime is already gone by now — the handler
                // revoked it synchronously before emitting.
                SystemEvent::UserActiveChanged { user_id, active } => {
                    let result = if active {
                        skald.container().ensure(&user_id).await
                    } else {
                        skald.container().stop(&user_id).await
                    };
                    if let Err(e) = result {
                        warn!(user = %user_id, active, error = %e,
                            "user-lifecycle: failed to apply active-state change to container");
                    }
                }
                SystemEvent::UserMountsChanged { user_id } => {
                    if let Err(e) = skald.refresh_user_mounts(&user_id).await {
                        warn!(user = %user_id, error = %e,
                            "user-lifecycle: remount failed (settles at next login/boot)");
                    }
                }
                // Both are pure appearance/metadata refreshes across live users —
                // they widen or re-sync what is visible, never narrow it, which is
                // what makes them safe to hand to a best-effort bus.
                SystemEvent::McpGlobalServersChanged => {
                    skald.refresh_global_mcp_access().await;
                }
                SystemEvent::ConnectorReinstalled { catalog_name } => {
                    skald.refresh_connector_after_reinstall(&catalog_name).await;
                }
                _ => {}
            }
        }
        info!("user-lifecycle: reconciler stopped");
    });
}

/// Spawns the **system-agent scheduler** — the instance-wide timer that runs the
/// background agents nobody asked for (today: TIC).
///
/// One loop, not one per user. Every pass walks the user directory and runs the
/// agent for each user **sequentially**: a pass means N container round-trips and
/// N LLM calls, and doing them concurrently would spike the box every interval
/// for no gain — nobody is waiting on a background tick.
///
/// A user whose database is still locked is **skipped**, and that is the normal
/// case rather than an error: the pool is the unlock token (§9), so a user who
/// has not logged in since the last restart has no readable events, no session
/// store, and no place to record the skip. It is logged at INFO and the pass
/// moves on; their events keep accumulating and are picked up by the first pass
/// after they log in.
///
/// Spawned after `Skald` is fully built, like [`spawn_user_lifecycle`] and for
/// the same reason: it resolves each user's runtime through `Skald::user_context`.
/// The back-reference is [`std::sync::Weak`].
pub(super) fn spawn_system_agents(skald: &Arc<super::Skald>, tic_config: TicConfig) {
    let weak       = Arc::downgrade(skald);
    let shutdown   = skald.rt.shutdown_token.clone();
    let mut sys_rx = skald.rt.system_bus.subscribe();

    let tic = TicManager::new(
        tic_config,
        Arc::clone(&skald.rt.config),
        Arc::clone(&skald.rt.db),
    );

    skald.rt.supervisor.spawn("system-agents", async move {
        info!("system-agents: scheduler started");

        'outer: loop {
            // Re-read the interval each pass so a Settings change lands without a
            // restart; a live change also cuts the current wait short.
            let wait = Duration::from_secs(tic.interval_secs().await);
            let deadline = tokio::time::sleep(wait);
            tokio::pin!(deadline);

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break 'outer,
                    _ = &mut deadline       => break,
                    ev = sys_rx.recv() => match ev {
                        Ok(SystemEvent::ConfigKeyUpdated { key, .. })
                            if key == TIC_INTERVAL_MINUTES_KEY =>
                        {
                            info!("system-agents: interval changed, rescheduling");
                            continue 'outer;
                        }
                        Err(RecvError::Closed) => break 'outer,
                        _ => {}
                    },
                }
            }

            let Some(skald) = weak.upgrade() else { break };
            tic_pass(&skald, &tic).await;
        }

        info!("system-agents: scheduler stopped");
    });
}

/// One TIC pass over the whole directory, one user at a time.
async fn tic_pass(skald: &Arc<super::Skald>, tic: &Arc<TicManager>) {
    if !tic.is_enabled().await {
        return;
    }

    let users = match skald.users().list().await {
        Ok(u) => u,
        Err(e) => {
            warn!(error = %e, "system-agents: cannot list users, skipping this pass");
            return;
        }
    };

    for user in users.into_iter().filter(|u| u.active) {
        if skald.rt.shutdown_token.is_cancelled() {
            break;
        }

        if !skald.users().is_unlocked(&user.id) {
            info!(
                user = %user.id, username = %user.username,
                "TIC: skipped — the user's database is still encrypted (not logged in since the last restart)",
            );
            continue;
        }

        // Unlocked, so this resolves (and is normally already live from their login).
        let Some(ctx) = skald.user_context(&user.id).await else {
            warn!(user = %user.id, "TIC: skipped — could not resolve the user's runtime");
            continue;
        };

        if let Err(e) = tic.run_for(&user.id, &ctx.pool, &ctx.sessions, &ctx.chat_hub).await {
            // One user's failure must not end the pass for everyone after them.
            warn!(user = %user.id, error = %e, "TIC: tick failed");
        }
    }
}
