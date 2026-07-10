use crate::{
    client::HonchoClient,
    error::Result,
    models::*,
    workspaces::page_query,
};

impl HonchoClient {
    // ── CRUD ──────────────────────────────────────────────────────────────

    pub async fn create_peer(&self, workspace_id: &str, body: &PeerCreate) -> Result<Peer> {
        self.post(&format!("/v3/workspaces/{workspace_id}/peers"), body)
            .await
    }

    pub async fn list_peers(
        &self,
        workspace_id: &str,
        params: &PageParams,
        filter: &PeerGet,
    ) -> Result<Page<Peer>> {
        self.post_with_query(
            &format!("/v3/workspaces/{workspace_id}/peers/list"),
            &page_query(params),
            filter,
        )
        .await
    }

    pub async fn update_peer(
        &self,
        workspace_id: &str,
        peer_id: &str,
        body: &PeerUpdate,
    ) -> Result<Peer> {
        self.put(
            &format!("/v3/workspaces/{workspace_id}/peers/{peer_id}"),
            body,
        )
        .await
    }

    // ── Sessions for a peer ───────────────────────────────────────────────

    pub async fn list_peer_sessions(
        &self,
        workspace_id: &str,
        peer_id: &str,
        params: &PageParams,
        filter: &SessionGet,
    ) -> Result<Page<Session>> {
        self.post_with_query(
            &format!("/v3/workspaces/{workspace_id}/peers/{peer_id}/sessions"),
            &page_query(params),
            filter,
        )
        .await
    }

    // ── Dialectic (memory query via LLM) ──────────────────────────────────

    /// Query a peer's memory using natural language. Returns the LLM answer.
    /// Note: `stream: true` is not supported by this client (use raw reqwest if needed).
    pub async fn peer_chat(
        &self,
        workspace_id: &str,
        peer_id: &str,
        opts: &DialecticOptions,
    ) -> Result<serde_json::Value> {
        self.post(
            &format!("/v3/workspaces/{workspace_id}/peers/{peer_id}/chat"),
            opts,
        )
        .await
    }

    // ── Representation ────────────────────────────────────────────────────

    /// Get a structured representation of a peer's memory.
    pub async fn peer_representation(
        &self,
        workspace_id: &str,
        peer_id: &str,
        opts: &PeerRepresentationGet,
    ) -> Result<serde_json::Value> {
        self.post(
            &format!("/v3/workspaces/{workspace_id}/peers/{peer_id}/representation"),
            opts,
        )
        .await
    }

    // ── Context ───────────────────────────────────────────────────────────

    /// Get context for a peer (conclusions + card, ready for injection into prompts).
    pub async fn peer_context(
        &self,
        workspace_id: &str,
        peer_id: &str,
        opts: &PeerRepresentationGet,
    ) -> Result<serde_json::Value> {
        let mut q: Vec<(&str, String)> = vec![];
        if let Some(ref v) = opts.target {
            q.push(("target", v.clone()));
        }
        if let Some(ref v) = opts.search_query {
            q.push(("search_query", v.clone()));
        }
        if let Some(v) = opts.search_top_k {
            q.push(("search_top_k", v.to_string()));
        }
        if let Some(v) = opts.search_max_distance {
            q.push(("search_max_distance", v.to_string()));
        }
        if let Some(v) = opts.include_most_frequent {
            q.push(("include_most_frequent", v.to_string()));
        }
        if let Some(v) = opts.max_conclusions {
            q.push(("max_conclusions", v.to_string()));
        }
        self.get_with_query(
            &format!("/v3/workspaces/{workspace_id}/peers/{peer_id}/context"),
            &q,
        )
        .await
    }

    // ── Card ──────────────────────────────────────────────────────────────

    pub async fn get_peer_card(
        &self,
        workspace_id: &str,
        peer_id: &str,
        target: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut q: Vec<(&str, String)> = vec![];
        if let Some(t) = target {
            q.push(("target", t.to_owned()));
        }
        self.get_with_query(
            &format!("/v3/workspaces/{workspace_id}/peers/{peer_id}/card"),
            &q,
        )
        .await
    }

    pub async fn set_peer_card(
        &self,
        workspace_id: &str,
        peer_id: &str,
        target: Option<&str>,
        card: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let mut q: Vec<(&str, String)> = vec![];
        if let Some(t) = target {
            q.push(("target", t.to_owned()));
        }
        self.put_with_query(
            &format!("/v3/workspaces/{workspace_id}/peers/{peer_id}/card"),
            &q,
            &card,
        )
        .await
    }

    // ── Search ────────────────────────────────────────────────────────────

    pub async fn search_peer(
        &self,
        workspace_id: &str,
        peer_id: &str,
        opts: &MessageSearchOptions,
    ) -> Result<Vec<Message>> {
        self.post(
            &format!("/v3/workspaces/{workspace_id}/peers/{peer_id}/search"),
            opts,
        )
        .await
    }
}
