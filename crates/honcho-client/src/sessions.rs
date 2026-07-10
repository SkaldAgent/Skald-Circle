use std::collections::HashMap;

use crate::{
    client::HonchoClient,
    error::Result,
    models::*,
    workspaces::page_query,
};

impl HonchoClient {
    // ── CRUD ──────────────────────────────────────────────────────────────

    pub async fn create_session(
        &self,
        workspace_id: &str,
        body: &SessionCreate,
    ) -> Result<Session> {
        self.post(&format!("/v3/workspaces/{workspace_id}/sessions"), body)
            .await
    }

    pub async fn list_sessions(
        &self,
        workspace_id: &str,
        params: &PageParams,
        filter: &SessionGet,
    ) -> Result<Page<Session>> {
        self.post_with_query(
            &format!("/v3/workspaces/{workspace_id}/sessions/list"),
            &page_query(params),
            filter,
        )
        .await
    }

    pub async fn update_session(
        &self,
        workspace_id: &str,
        session_id: &str,
        body: &SessionUpdate,
    ) -> Result<Session> {
        self.put(
            &format!("/v3/workspaces/{workspace_id}/sessions/{session_id}"),
            body,
        )
        .await
    }

    pub async fn delete_session(&self, workspace_id: &str, session_id: &str) -> Result<()> {
        self.delete_ok(&format!(
            "/v3/workspaces/{workspace_id}/sessions/{session_id}"
        ))
        .await
    }

    /// Clone a session, optionally up to a specific message.
    pub async fn clone_session(
        &self,
        workspace_id: &str,
        session_id: &str,
        up_to_message_id: Option<&str>,
    ) -> Result<Session> {
        let q: Vec<(&str, String)> = up_to_message_id
            .map(|mid| vec![("message_id", mid.to_owned())])
            .unwrap_or_default();
        self.post_with_query(
            &format!("/v3/workspaces/{workspace_id}/sessions/{session_id}/clone"),
            &q,
            &serde_json::Value::Null, // no body
        )
        .await
    }

    // ── Peers in session ──────────────────────────────────────────────────

    /// Add peers to a session.
    pub async fn add_session_peers(
        &self,
        workspace_id: &str,
        session_id: &str,
        peers: &HashMap<String, SessionPeerConfig>,
    ) -> Result<serde_json::Value> {
        self.post(
            &format!("/v3/workspaces/{workspace_id}/sessions/{session_id}/peers"),
            peers,
        )
        .await
    }

    /// Update peer configs in a session.
    pub async fn update_session_peers(
        &self,
        workspace_id: &str,
        session_id: &str,
        peers: &HashMap<String, SessionPeerConfig>,
    ) -> Result<serde_json::Value> {
        self.put(
            &format!("/v3/workspaces/{workspace_id}/sessions/{session_id}/peers"),
            peers,
        )
        .await
    }

    /// Remove peers from a session by their ids.
    pub async fn remove_session_peers(
        &self,
        workspace_id: &str,
        session_id: &str,
        peer_ids: &[&str],
    ) -> Result<()> {
        self.delete_json(
            &format!("/v3/workspaces/{workspace_id}/sessions/{session_id}/peers"),
            &peer_ids,
        )
        .await
    }

    /// List peers in a session.
    pub async fn list_session_peers(
        &self,
        workspace_id: &str,
        session_id: &str,
        params: &PageParams,
    ) -> Result<Page<Peer>> {
        self.get_with_query(
            &format!("/v3/workspaces/{workspace_id}/sessions/{session_id}/peers"),
            &page_query(params),
        )
        .await
    }

    /// Get config for a specific peer in a session.
    pub async fn get_session_peer_config(
        &self,
        workspace_id: &str,
        session_id: &str,
        peer_id: &str,
    ) -> Result<SessionPeerConfig> {
        self.get(&format!(
            "/v3/workspaces/{workspace_id}/sessions/{session_id}/peers/{peer_id}/config"
        ))
        .await
    }

    /// Update config for a specific peer in a session.
    pub async fn update_session_peer_config(
        &self,
        workspace_id: &str,
        session_id: &str,
        peer_id: &str,
        config: &SessionPeerConfig,
    ) -> Result<SessionPeerConfig> {
        self.put(
            &format!(
                "/v3/workspaces/{workspace_id}/sessions/{session_id}/peers/{peer_id}/config"
            ),
            config,
        )
        .await
    }

    // ── Context / Summaries ───────────────────────────────────────────────

    /// Retrieve context for a session (messages + peer conclusions, token-budgeted).
    pub async fn session_context(
        &self,
        workspace_id: &str,
        session_id: &str,
        tokens: Option<u32>,
        search_query: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut q: Vec<(&str, String)> = vec![];
        if let Some(t) = tokens {
            q.push(("tokens", t.to_string()));
        }
        if let Some(sq) = search_query {
            q.push(("search_query", sq.to_owned()));
        }
        self.get_with_query(
            &format!("/v3/workspaces/{workspace_id}/sessions/{session_id}/context"),
            &q,
        )
        .await
    }

    pub async fn session_summaries(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<serde_json::Value> {
        self.get(&format!(
            "/v3/workspaces/{workspace_id}/sessions/{session_id}/summaries"
        ))
        .await
    }

    // ── Search ────────────────────────────────────────────────────────────

    pub async fn search_session(
        &self,
        workspace_id: &str,
        session_id: &str,
        opts: &MessageSearchOptions,
    ) -> Result<Vec<Message>> {
        self.post(
            &format!("/v3/workspaces/{workspace_id}/sessions/{session_id}/search"),
            opts,
        )
        .await
    }
}
