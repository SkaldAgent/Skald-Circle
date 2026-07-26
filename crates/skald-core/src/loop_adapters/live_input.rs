//! `PendingUserInput` → the crate's `LiveInput` (D10 pull-based live input).

use std::sync::Arc;

use agent_loop::manager::LiveInput;
use agent_loop::store::NewMessage;

use crate::session::handler::PendingUserInput;

/// Drains the source's inbox into the running turn: one `NewMessage` per
/// queued user message, attachments/command metadata preserved.
pub struct PendingLiveInput {
    inner: Arc<dyn PendingUserInput>,
}

impl PendingLiveInput {
    pub fn new(inner: Arc<dyn PendingUserInput>) -> Self { Self { inner } }
}

#[agent_loop::async_trait]
impl LiveInput for PendingLiveInput {
    async fn drain(&self) -> Vec<NewMessage> {
        self.inner
            .drain_user()
            .await
            .into_iter()
            .map(|m| {
                let mut msg = NewMessage::user(m.content);
                if let Some(meta) = m.metadata
                    && let Ok(v) = serde_json::to_value(meta)
                {
                    msg.metadata = Some(v);
                }
                msg
            })
            .collect()
    }
}
