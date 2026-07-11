//! Per-user event forwarders (blueprint §13), the mobile analogue of the Telegram
//! `ensure_forwarder`.
//!
//! One forwarder per unlocked user with bound devices subscribes to that user's
//! event stream ([`UserChannelHandle::subscribe`]) and routes the six Inbox
//! lifecycle events through the user's [`DelayedNotifier`], which decides whether
//! and when to push the Inbox to their phones.
//!
//! Unlike the Telegram forwarder there is **no `source` filter**: an approval
//! raised in the user's *web* session must still reach their phone. The relevant
//! events (`{Approval,Clarification,Elicitation}{Requested,Resolved}`) are
//! Inbox-scoped and carry request ids from the user's own pool, so no cross-user
//! collision is possible.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use core_api::events::ServerEvent;
use core_api::user_channel::UserChannelHandle;

use crate::PLUGIN_ID;
use crate::app::RelayApp;
use crate::notifier::{DelayedNotifier, Kind};

/// How often the reconcile loop picks up bound users who have unlocked since the
/// last pass. Pushes are best-effort, so a coarse cadence is fine.
const RECONCILE_INTERVAL: Duration = Duration::from_secs(60);

/// Periodically (re)spawns forwarders for bound + unlocked users.
///
/// This is load-bearing, not a nicety: at boot every pool is locked (§9), so the
/// eager start-time pass spawns nothing. Users unlock later via web/phone login,
/// and there is no "user unlocked" system event to hook. Without this loop a user
/// whose phone stays backgrounded would never get a forwarder — so no Inbox push
/// would ever be armed for them. `ensure_forwarder` dedups, so this is idempotent
/// and cheap (locked users resolve to `None` and are skipped without a build).
pub(crate) async fn reconcile_loop(app: Arc<RelayApp>) {
    let cancel = app.cancel();
    let mut ticker = tokio::time::interval(RECONCILE_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = ticker.tick() => spawn_forwarders_for_bound_users(&app).await,
        }
    }
}

/// Spawns forwarders for every bound user whose context is already unlocked.
/// Called at plugin start, on binding changes, and by the reconcile loop. Users
/// who unlock later also get their forwarder spawned lazily on device activity.
pub(crate) async fn spawn_forwarders_for_bound_users(app: &Arc<RelayApp>) {
    let user_ids = app.bindings.read().await.bound_user_ids();
    for user_id in user_ids {
        if let Some(handle) = app.user_channel.resolve_user(&user_id).await {
            ensure_forwarder(Arc::clone(app), user_id, handle).await;
        }
    }
}

/// Spawns a per-user forwarder if one is not already running for `user_id`.
pub(crate) async fn ensure_forwarder(
    app: Arc<RelayApp>,
    user_id: String,
    handle: Arc<dyn UserChannelHandle>,
) {
    {
        let mut forwarders = app.forwarders.lock().await;
        if !forwarders.insert(user_id.clone()) {
            return;
        }
    }
    let cancel = app.cancel();
    info!(plugin = PLUGIN_ID, user_id = %user_id, "spawning per-user forwarder");
    tokio::spawn(user_forwarder(app, user_id, handle, cancel));
}

/// One forwarder per unlocked user. Subscribes to the user's event stream and
/// drives their `DelayedNotifier`. Exits when the stream closes (user context
/// dropped at restart / lock) or the plugin is cancelled — self-cleaning.
async fn user_forwarder(
    app: Arc<RelayApp>,
    user_id: String,
    handle: Arc<dyn UserChannelHandle>,
    cancel: CancellationToken,
) {
    // Get or create this user's debounced notifier.
    let notifier: Arc<DelayedNotifier> = {
        let mut notifiers = app.notifiers.lock().await;
        notifiers
            .entry(user_id.clone())
            .or_insert_with(|| DelayedNotifier::new(Arc::downgrade(&app), user_id.clone(), app.notify_delay))
            .clone()
    };

    let mut rx = handle.subscribe();
    loop {
        let event: ServerEvent = tokio::select! {
            _ = cancel.cancelled() => break,
            result = rx.recv() => match result {
                Ok(ge) => ge.event,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(plugin = PLUGIN_ID, user_id = %user_id, skipped = n, "forwarder lagged");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!(plugin = PLUGIN_ID, user_id = %user_id, "forwarder — user context closed, exiting");
                    break;
                }
            },
        };

        // `*Requested` arms a delayed push; `*Resolved` cancels it (or refreshes
        // the phone if the push already went out). Any other event is ignored.
        match event {
            ServerEvent::ApprovalRequested { request_id, .. } => {
                notifier.on_requested((Kind::Approval, request_id)).await;
            }
            ServerEvent::ApprovalResolved { request_id, .. } => {
                notifier.on_resolved((Kind::Approval, request_id)).await;
            }
            ServerEvent::ClarificationRequested { request_id, .. } => {
                notifier.on_requested((Kind::Clarification, request_id)).await;
            }
            ServerEvent::ClarificationResolved { request_id } => {
                notifier.on_resolved((Kind::Clarification, request_id)).await;
            }
            ServerEvent::ElicitationRequested { request_id, .. } => {
                notifier.on_requested((Kind::Elicitation, request_id)).await;
            }
            ServerEvent::ElicitationResolved { request_id } => {
                notifier.on_resolved((Kind::Elicitation, request_id)).await;
            }
            _ => {}
        }
    }

    // Clean up so a later reconnect can respawn.
    app.forwarders.lock().await.remove(&user_id);
    app.notifiers.lock().await.remove(&user_id);
    info!(plugin = PLUGIN_ID, user_id = %user_id, "forwarder exited");
}
