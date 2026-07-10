//! Delayed push notifier.
//!
//! The mobile push for an Inbox item is only valuable when the user is *away*
//! from the computer. When they're sitting at the chat, every approval /
//! clarification would otherwise fire an instant — and pointless — phone
//! notification, since they'll answer on the computer within seconds.
//!
//! `DelayedNotifier` debounces that: when a request enters the Inbox it starts a
//! timer; only if the request is still unresolved after `delay` does it push
//! (`broadcast_inbox`). If the user resolves it on the computer first, the timer
//! is cancelled and **no** push is sent. Once a push *has* gone out, the eventual
//! resolution is broadcast so the phone clears the item.
//!
//! Elicitations are the exception: they live only in the Inbox (never inline in
//! the chat), so there is no computer-side answer to debounce against and they
//! are pushed immediately regardless of `delay`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::PLUGIN_ID;
use crate::app::RelayApp;

/// Which Inbox manager a `request_id` belongs to. Approvals and clarifications
/// use independent atomic counters, so the id alone is not unique.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Kind {
    Approval,
    Clarification,
    Elicitation,
}

/// `(kind, request_id)` — unique across both Inbox managers.
type Key = (Kind, i64);

#[derive(Default)]
struct State {
    /// Requested, timer armed, push not yet sent.
    pending: HashMap<Key, CancellationToken>,
    /// Push already sent, awaiting the resolution that clears the phone item.
    notified: HashSet<Key>,
}

/// Debounces Inbox pushes to the phone. Cheap to clone (`Arc` inside).
pub struct DelayedNotifier {
    app: Arc<RelayApp>,
    delay: Duration,
    state: Mutex<State>,
}

impl DelayedNotifier {
    pub fn new(app: Arc<RelayApp>, delay: Duration) -> Arc<Self> {
        Arc::new(Self { app, delay, state: Mutex::new(State::default()) })
    }

    /// A request entered the Inbox: arm a timer. If `delay` elapses before a
    /// matching `on_resolved`, push the Inbox to the phone.
    ///
    /// Exception: elicitations (e.g. MCP password prompts) live *only* in the
    /// Inbox — never inline in the chat — so there is no computer-side answer to
    /// debounce against. Waiting `delay` would just delay a push that is always
    /// warranted, so they are pushed immediately (marked *notified* so the
    /// eventual `on_resolved` refreshes the phone to clear the item).
    pub async fn on_requested(self: &Arc<Self>, key: Key) {
        if key.0 == Kind::Elicitation {
            let newly_notified = {
                let mut st = self.state.lock().await;
                // Drop any stale armed timer, then mark notified.
                if let Some(old) = st.pending.remove(&key) {
                    old.cancel();
                }
                st.notified.insert(key)
            };
            if newly_notified {
                if let Err(e) = self.app.broadcast_inbox().await {
                    warn!(plugin = PLUGIN_ID, error = %e, "immediate elicitation push failed");
                }
            }
            return;
        }

        let token = CancellationToken::new();
        {
            let mut st = self.state.lock().await;
            // Re-arming an already-pending key: cancel the stale timer first.
            if let Some(old) = st.pending.insert(key, token.clone()) {
                old.cancel();
            }
        }

        let this = Arc::clone(self);
        let delay = self.delay;
        tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => {} // resolved within the window — send nothing
                _ = sleep(delay) => {
                    // Promote pending → notified under the lock, then push.
                    let still_armed = {
                        let mut st = this.state.lock().await;
                        if st.pending.remove(&key).is_some() {
                            st.notified.insert(key);
                            true
                        } else {
                            false
                        }
                    };
                    if still_armed {
                        if let Err(e) = this.app.broadcast_inbox().await {
                            warn!(plugin = PLUGIN_ID, error = %e, "delayed inbox push failed");
                        }
                    }
                }
            }
        });
    }

    /// A request was resolved (approved / rejected / answered). Suppress the push
    /// if it was still pending; otherwise refresh the phone so it clears the item.
    pub async fn on_resolved(&self, key: Key) {
        let broadcast = {
            let mut st = self.state.lock().await;
            if let Some(token) = st.pending.remove(&key) {
                token.cancel(); // push hadn't fired yet — suppress entirely
                false
            } else {
                // Push already went out, or key untracked: refresh the snapshot.
                st.notified.remove(&key);
                true
            }
        };
        if broadcast {
            if let Err(e) = self.app.broadcast_inbox().await {
                warn!(plugin = PLUGIN_ID, error = %e, "inbox broadcast failed");
            }
        }
    }

    /// Cancel every armed timer (called on plugin stop).
    pub async fn cancel_all(&self) {
        let mut st = self.state.lock().await;
        for (_, token) in st.pending.drain() {
            token.cancel();
        }
        st.notified.clear();
    }
}
