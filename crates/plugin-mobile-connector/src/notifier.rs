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
//!
//! # Per-user (blueprint §13)
//!
//! There is one `DelayedNotifier` **per user** (owned by that user's forwarder),
//! so the `(kind, request_id)` keyspace is naturally scoped — request ids drawn
//! from different user pools never collide. On fire the notifier pushes only that
//! user's Inbox, via a `Weak` back-reference to the shared [`RelayApp`] (weak to
//! avoid an `Arc` cycle: `RelayApp` holds the notifiers).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Weak};
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

/// Debounces Inbox pushes to one user's phones.
pub struct DelayedNotifier {
    /// Weak to break the `RelayApp` → notifiers → `RelayApp` cycle.
    app: Weak<RelayApp>,
    user_id: String,
    delay: Duration,
    state: Mutex<State>,
}

impl DelayedNotifier {
    pub fn new(app: Weak<RelayApp>, user_id: String, delay: Duration) -> Arc<Self> {
        Arc::new(Self { app, user_id, delay, state: Mutex::new(State::default()) })
    }

    /// Push this user's Inbox to their phones, if the app is still alive.
    async fn push(&self) {
        if let Some(app) = self.app.upgrade() {
            if let Err(e) = app.push_inbox_to_user(&self.user_id).await {
                warn!(plugin = PLUGIN_ID, error = %e, "inbox push failed");
            }
        }
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
                self.push().await;
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
                        this.push().await;
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
            self.push().await;
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
