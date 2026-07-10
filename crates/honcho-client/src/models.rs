use serde::{Deserialize, Serialize};
use serde_json::Value;

// ──────────────────────────────────────────
// Pagination
// ──────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: u64,
    pub page: u64,
    pub size: u64,
    pub pages: u64,
}

// ──────────────────────────────────────────
// Workspace
// ──────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub metadata: Option<Value>,
    pub configuration: Option<Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct WorkspaceCreate {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct WorkspaceUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Value>,
}

/// Filter body for `POST /workspaces/list`
#[derive(Debug, Clone, Serialize, Default)]
pub struct WorkspaceGet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<Value>,
}

// ──────────────────────────────────────────
// Peer
// ──────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Peer {
    pub id: String,
    pub workspace_id: String,
    pub created_at: String,
    pub metadata: Option<Value>,
    pub configuration: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PeerCreate {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PeerUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PeerGet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<Value>,
}

// ──────────────────────────────────────────
// Session
// ──────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    pub id: String,
    pub is_active: bool,
    pub workspace_id: String,
    pub metadata: Option<Value>,
    pub configuration: Option<Value>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SessionCreate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Map of peer_id → SessionPeerConfig
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peers: Option<std::collections::HashMap<String, SessionPeerConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SessionUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SessionGet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionPeerConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe_me: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe_others: Option<bool>,
}

// ──────────────────────────────────────────
// Message
// ──────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub id: String,
    pub content: String,
    pub peer_id: String,
    pub session_id: String,
    pub workspace_id: String,
    pub metadata: Option<Value>,
    pub created_at: String,
    pub token_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageCreate {
    pub content: String,
    pub peer_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Value>,
    /// RFC3339 datetime; if None the server assigns now
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessageBatchCreate {
    pub messages: Vec<MessageCreate>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MessageGet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MessageUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

// ──────────────────────────────────────────
// Conclusion
// ──────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Conclusion {
    pub id: String,
    pub content: String,
    pub observer_id: String,
    pub observed_id: String,
    pub session_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConclusionCreate {
    /// 1–65535 characters
    pub content: String,
    pub observer_id: String,
    pub observed_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConclusionBatchCreate {
    /// 1–100 conclusions
    pub conclusions: Vec<ConclusionCreate>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ConclusionGet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConclusionQuery {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    /// 0.0–1.0
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<Value>,
}

// ──────────────────────────────────────────
// Dialectic / Peer context
// ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct DialecticOptions {
    /// Natural language query
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// peer_id to get the representation for (defaults to the caller)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// minimal | low | medium | high | max
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_level: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PeerRepresentationGet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_query: Option<String>,
    /// 1–100
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_top_k: Option<u32>,
    /// 0.0–1.0
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_max_distance: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_most_frequent: Option<bool>,
    /// 1–100
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_conclusions: Option<u32>,
}

// ──────────────────────────────────────────
// Search
// ──────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct MessageSearchOptions {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

// ──────────────────────────────────────────
// Queue
// ──────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct QueueStatus {
    pub total_work_units: u64,
    pub completed_work_units: u64,
    pub in_progress_work_units: u64,
    pub pending_work_units: u64,
    pub sessions: Option<Value>,
}

// ──────────────────────────────────────────
// List query params (pagination)
// ──────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct PageParams {
    pub page: Option<u64>,
    pub size: Option<u64>,
    pub reverse: Option<bool>,
}
