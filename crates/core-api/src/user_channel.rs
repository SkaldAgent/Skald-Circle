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
use crate::inbox::InboxApi;

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

    /// Whether `user_id` may currently use the plugin `plugin_id` — granted in
    /// `plugin_access`, or holding the admin role (implicit, mirroring the web
    /// `/plugins/mine` view). Channel adapters enforce this on every inbound
    /// message so an admin revoking access takes effect immediately, without
    /// having to touch existing pairing/binding rows. **Fail-closed**: an
    /// unknown user or a lookup error returns `false`.
    async fn plugin_access(&self, plugin_id: &str, user_id: &str) -> bool;

    /// Resolves a web **session token** to its user id, or `None` if the token
    /// is unknown / expired. Lets a channel adapter turn a token the client
    /// obtained from `POST /api/auth/login` into an authenticated identity — the
    /// seam behind the mobile self-service device binding (a device proves *who*
    /// it is by presenting the session it just logged in with). Sessions are
    /// in-memory, so a token stops resolving after a restart.
    async fn user_for_session(&self, token: &str) -> Option<String>;
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

    /// The user's Inbox — the unified view over pending approvals,
    /// clarifications and MCP elicitations. Channel adapters that bridge the
    /// whole Inbox (e.g. the mobile connector) use this instead of wiring
    /// `approval()`/clarification/elicitation separately.
    fn inbox(&self) -> Arc<dyn InboxApi>;

    /// Subscribe to the user's server→client event stream.
    /// Events are scoped to this user; no cross-user leakage.
    fn subscribe(&self) -> broadcast::Receiver<GlobalEvent>;
}
