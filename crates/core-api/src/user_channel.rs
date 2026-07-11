//! Channel-to-session contract (blueprint §13).
//!
//! In the multi-user architecture, chat hubs, approval managers and event
//! streams are per-user (inside [`UserContext`]). External channels (Telegram,
//! mobile, …) need a way to resolve a user's owner-bound runtime at runtime,
//! without depending on the concrete `Skald` / `UserContext` types.
//!
//! [`UserChannelApi`] is the lookup seam: given a `user_id`, returns a
//! [`UserChannelHandle`] when the user's database is unlocked (§9), or `None`
//! when it is still locked. The handle exposes the per-user [`ChatHubApi`],
//! [`ApprovalApi`] and event stream — everything a channel adapter needs to
//! route a message and receive the response events.
//!
//! This is the "one contract" of §13: each channel (Telegram, mobile, …) is a
//! thin adapter over it, so N channel rewrites become 1 contract + N adapters.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::approval::ApprovalApi;
use crate::chat_hub::ChatHubApi;
use crate::events::GlobalEvent;

/// Resolves an unlocked user's channel handle.
///
/// Implemented by the application core (`Skald`) and injected into
/// [`crate::plugin::PluginContext`] as `user_channel`.
#[async_trait]
pub trait UserChannelApi: Send + Sync {
    /// Returns the user's handle if their database is unlocked in this boot
    /// (§9: from first login until restart). `None` = locked — the caller
    /// should prompt the user to log in.
    async fn resolve_user(&self, user_id: &str) -> Option<Arc<dyn UserChannelHandle>>;
}

/// Handle to one unlocked user's owner-bound runtime.
///
/// Lifetime = the user's pool lifetime (§9). Cloning the returned `Arc`s is
/// cheap (they share the underlying state). The event receiver obtained from
/// [`UserChannelHandle::subscribe`] is independent per call — each subscriber
/// gets every future event.
pub trait UserChannelHandle: Send + Sync {
    /// The opaque user id this handle belongs to.
    fn user_id(&self) -> &str;

    /// The user's chat hub — send messages, manage sessions, query context.
    fn chat_hub(&self) -> Arc<dyn ChatHubApi>;

    /// The user's approval manager — resolve pending tool-call approvals.
    fn approval(&self) -> Arc<dyn ApprovalApi>;

    /// Subscribe to the user's server→client event stream.
    /// Events are scoped to this user; no cross-user leakage.
    fn subscribe(&self) -> broadcast::Receiver<GlobalEvent>;
}
