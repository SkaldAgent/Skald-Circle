//! Post-construction wiring: the `OnceLock` cycle-breakers and the background-task
//! spawns, each concentrated in one readable place instead of being scattered
//! through the constructor.

use std::sync::Arc;

use tracing::info;

use crate::core::config::CoreConfig;
use crate::core::elicitation::ElicitationBridge;

use super::bundles::{Conversation, Integrations, Interaction, Tasks};
use super::runtime::Runtime;

/// Resolves the construction cycles (`cron ↔ session ↔ hub`, `ticket → cron`,
/// `mcp → elicitation`) via the managers' `OnceLock` setters.
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

/// Spawns every long-lived background task, each registered by name with the
/// supervisor so it is joined on shutdown. MCP `initialize()` is spawned here —
/// after `wire()` has installed the elicitation handler — so stdio servers start
/// with a handler for server-initiated `elicitation/create` requests.
pub(super) fn spawn_background(
    rt: &Runtime,
    tasks: &Tasks,
    conversation: &Conversation,
    integrations: &Integrations,
    config: &CoreConfig,
) {
    // LLM request-log retention/cleanup — first run 1 min after startup, then 12h.
    if let Some(cfg) = config.llm.requests_log.clone().filter(|r| r.enabled) {
        rt.supervisor.adopt_one(
            "llm-log-cleanup",
            crate::core::db::llm_requests::cleanup::spawn(
                Arc::clone(&rt.db),
                cfg,
                rt.shutdown_token.clone(),
            ),
        );
    }

    // Session-cancellation subscriber: fans SessionCancelled events on the system
    // bus into cancel_session() so any in-flight turn / approval / clarification
    // all unblock.
    {
        let manager_ref = Arc::clone(&conversation.manager);
        let mut rx = rt.system_bus.subscribe();
        let sd = rt.shutdown_token.clone();
        rt.supervisor.spawn("session-cancel", async move {
            loop {
                tokio::select! {
                    _ = sd.cancelled() => break,
                    event = rx.recv() => match event {
                        Ok(core_api::system_bus::SystemEvent::SessionCancelled { session_id }) => {
                            manager_ref.cancel_session(session_id).await;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
            }
        });
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

    rt.supervisor.adopt("cron", Arc::clone(&tasks.cron).start(rt.shutdown_token.clone()));
    info!("cron scheduler started");
    rt.supervisor.adopt_one(
        "ticket-listener",
        Arc::clone(&tasks.ticket_manager).start_listener(Arc::clone(&rt.system_bus), rt.shutdown_token.clone()),
    );
    rt.supervisor.adopt_one("tic", Arc::clone(&conversation.tic_manager).start(rt.shutdown_token.clone()));
    info!("TicManager started");
}
