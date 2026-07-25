//! Cross-cutting runtime context.
//!
//! `Runtime` holds the primitives every domain bundle needs: the DB pool, the
//! global config manager, the two event buses, the server→client broadcast
//! channel, the shutdown token and the background-task supervisor. It is built
//! first and passed by reference into each bundle builder, so no bundle has to
//! depend on another purely to reach a shared primitive. This is also the natural
//! seam a future extracted `skald-core` crate would expose as its root context.

use std::sync::Arc;

use sqlx::SqlitePool;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::info;

use core_api::events::GlobalEvent;
use core_api::system_bus::SystemEventBus;

use crate::auth::SessionStore;
use crate::chat_event_bus::ChatEventBus;
use crate::config_store::GlobalConfigManager;
use crate::users::UserManager;

use super::supervisor::TaskSupervisor;

pub(super) struct Runtime {
    /// The registry pool (`system.db`). Still the only pool anything reads or
    /// writes: nothing has moved to per-user pools yet. `users` owns those.
    pub(super) db:                Arc<SqlitePool>,
    pub(super) users:             Arc<UserManager>,
    pub(super) sessions:          Arc<SessionStore>,
    pub(super) config:            Arc<GlobalConfigManager>,
    pub(super) config_properties: Vec<core_api::ConfigSet>,
    pub(super) system_bus:        Arc<SystemEventBus>,
    pub(super) event_bus:         Arc<ChatEventBus>,
    /// Server→client push channel (`ServerEvent` wrapped in `GlobalEvent`). Shared
    /// into approval / clarification / elicitation / chat_hub; consumed by the WS
    /// handlers. Hoisted here so it exists before the bundles that need it.
    pub(super) global_tx:         broadcast::Sender<GlobalEvent>,
    pub(super) shutdown_token:    CancellationToken,
    pub(super) supervisor:        Arc<TaskSupervisor>,
}

impl Runtime {
    /// Wires the cross-cutting primitives. Infallible.
    pub(super) fn bootstrap(pool: Arc<SqlitePool>) -> Self {
        let system_bus = Arc::new(SystemEventBus::new());
        info!("system event bus ready");

        let config = Arc::new(GlobalConfigManager::new(Arc::clone(&pool), Arc::clone(&system_bus)));

        let users = Arc::new(UserManager::new(Arc::clone(&pool)));
        let sessions = Arc::new(SessionStore::new(Arc::clone(&users)));

        let event_bus = Arc::new(ChatEventBus::new());
        info!("chat event bus ready");

        let (global_tx, _) = broadcast::channel::<GlobalEvent>(512);

        Runtime {
            db: pool,
            users,
            sessions,
            config,
            config_properties: vec![
                crate::i18n::config_set(),
                crate::tic::config_set(),
                crate::compactor::config_set(),
            ],
            system_bus,
            event_bus,
            global_tx,
            shutdown_token: CancellationToken::new(),
            supervisor: TaskSupervisor::new(),
        }
    }
}
