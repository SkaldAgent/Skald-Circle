//! Post-construction wiring: the `OnceLock` cycle-breakers and the background-task
//! spawns, each concentrated in one readable place instead of being scattered
//! through the constructor.
//!
//! Owner-bound background loops (cron, session-cancel, ticket-listener, tic) have
//! moved per-user into `UserContextFactory::build`. What remains here are the
//! instance-wide tasks: LLM-log cleanup on the registry pool, and MCP server
//! initialization.

use std::sync::Arc;

use tracing::info;

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
    tasks.ticket_manager.set_task_manager(Arc::clone(&tasks.cron));
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
