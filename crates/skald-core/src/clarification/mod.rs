use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use chrono::Utc;
use serde::Serialize;
use tokio::sync::{broadcast, oneshot};
use tracing::info;

use crate::events::{GlobalEvent, ServerEvent};
use crate::pending_registry::PendingRegistry;

#[derive(Debug, Clone, Serialize)]
pub struct PendingClarificationInfo {
    pub request_id:        i64,
    pub session_id:        i64,
    pub agent_id:          String,
    pub source:            String,
    pub context_label:     Option<String>,
    pub title:             String,
    pub question:          String,
    pub suggested_answers: Vec<String>,
    pub created_at:        String,
}

pub struct ClarificationManager {
    /// Shared pending-request plumbing (map + oneshot). Keyed by `request_id`.
    registry: PendingRegistry<PendingClarificationInfo, String>,
    next_id:  AtomicI64,
    /// Global event bus sender, mirroring `ApprovalManager`. Used to broadcast
    /// `ClarificationRequested` / `ClarificationResolved` so Inbox subscribers
    /// (e.g. the mobile-connector plugin) can re-snapshot.
    event_tx: broadcast::Sender<GlobalEvent>,
}

impl ClarificationManager {
    pub fn new(event_tx: broadcast::Sender<GlobalEvent>) -> Arc<Self> {
        Arc::new(Self {
            registry: PendingRegistry::new(),
            next_id:  AtomicI64::new(1),
            event_tx,
        })
    }

    pub async fn register(
        &self,
        session_id:        i64,
        agent_id:          &str,
        source:            &str,
        context_label:     Option<&str>,
        title:             &str,
        question:          &str,
        suggested_answers: Vec<String>,
    ) -> (i64, oneshot::Receiver<String>) {
        let request_id = self.next_id.fetch_add(1, Ordering::SeqCst);

        let info = PendingClarificationInfo {
            request_id,
            session_id,
            agent_id:      agent_id.to_string(),
            source:        source.to_string(),
            context_label: context_label.map(str::to_string),
            title:         title.to_string(),
            question:      question.to_string(),
            suggested_answers,
            created_at:    Utc::now().to_rfc3339(),
        };

        let rx = self.registry.insert(request_id, info).await;
        info!(session_id, agent = agent_id, source, request_id, "clarification: pending registered");
        // Broadcast on the global bus; counterpart of the per-session
        // `AgentQuestion` WS event.
        let _ = self.event_tx.send(GlobalEvent {
            source:     Some(source.to_string()),
            session_id: Some(session_id),
            event:      ServerEvent::ClarificationRequested {
                request_id,
                title: title.to_string(),
            },
        });
        (request_id, rx)
    }

    pub async fn resolve(&self, request_id: i64, answer: String) -> bool {
        match self.registry.resolve(request_id, answer).await {
            Some(info) => {
                info!(request_id, "clarification: resolved");
                let _ = self.event_tx.send(GlobalEvent {
                    source:     Some(info.source),
                    session_id: Some(info.session_id),
                    event:      ServerEvent::ClarificationResolved { request_id },
                });
                true
            }
            None => false,
        }
    }

    pub async fn list_pending(&self) -> Vec<PendingClarificationInfo> {
        let mut items = self.registry.list().await;
        items.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        items
    }

    pub async fn cancel_for_session(&self, session_id: i64) {
        self.registry.remove_where(|i| i.session_id == session_id).await;
    }
}
