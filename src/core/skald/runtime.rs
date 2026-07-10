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

use crate::core::chat_event_bus::ChatEventBus;
use crate::core::config_store::GlobalConfigManager;

use super::supervisor::TaskSupervisor;

pub(super) struct Runtime {
    pub(super) db:                Arc<SqlitePool>,
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
        let config = Arc::new(GlobalConfigManager::new(Arc::clone(&pool)));

        let system_bus = Arc::new(SystemEventBus::new());
        info!("system event bus ready");

        let event_bus = Arc::new(ChatEventBus::new());
        info!("chat event bus ready");

        let (global_tx, _) = broadcast::channel::<GlobalEvent>(512);

        Runtime {
            db: pool,
            config,
            config_properties: vec![crate::core::tic::config_set()],
            system_bus,
            event_bus,
            global_tx,
            shutdown_token: CancellationToken::new(),
            supervisor: TaskSupervisor::new(),
        }
    }
}
