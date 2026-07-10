use crate::{
    client::{parse_no_content, HonchoClient},
    error::Result,
    models::*,
};

impl HonchoClient {
    // ── CRUD ──────────────────────────────────────────────────────────────

    /// Create (or retrieve if already exists) a workspace.
    pub async fn create_workspace(&self, body: &WorkspaceCreate) -> Result<Workspace> {
        self.post("/v3/workspaces", body).await
    }

    /// List workspaces (admin only).
    pub async fn list_workspaces(
        &self,
        params: &PageParams,
        filter: &WorkspaceGet,
    ) -> Result<Page<Workspace>> {
        self.post_with_query("/v3/workspaces/list", &page_query(params), filter)
            .await
    }


    /// Update a workspace.
    pub async fn update_workspace(
        &self,
        workspace_id: &str,
        body: &WorkspaceUpdate,
    ) -> Result<Workspace> {
        self.put(&format!("/v3/workspaces/{workspace_id}"), body)
            .await
    }

    /// Delete a workspace (queued, async on server side).
    pub async fn delete_workspace(&self, workspace_id: &str) -> Result<()> {
        let url = self.url(&format!("/v3/workspaces/{workspace_id}"));
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await?;
        parse_no_content(resp).await
    }

    // ── Search ────────────────────────────────────────────────────────────

    pub async fn search_workspace(
        &self,
        workspace_id: &str,
        opts: &MessageSearchOptions,
    ) -> Result<Vec<Message>> {
        self.post(&format!("/v3/workspaces/{workspace_id}/search"), opts)
            .await
    }

    // ── Queue ─────────────────────────────────────────────────────────────

    pub async fn queue_status(
        &self,
        workspace_id: &str,
        observer_id: Option<&str>,
        sender_id: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<QueueStatus> {
        let mut q: Vec<(&str, String)> = vec![];
        if let Some(v) = observer_id {
            q.push(("observer_id", v.to_owned()));
        }
        if let Some(v) = sender_id {
            q.push(("sender_id", v.to_owned()));
        }
        if let Some(v) = session_id {
            q.push(("session_id", v.to_owned()));
        }
        self.get_with_query(&format!("/v3/workspaces/{workspace_id}/queue/status"), &q)
            .await
    }
}

// ── helpers ───────────────────────────────────────────────────────────────

pub(crate) fn page_query(p: &PageParams) -> Vec<(&'static str, String)> {
    let mut q = vec![];
    if let Some(v) = p.page {
        q.push(("page", v.to_string()));
    }
    if let Some(v) = p.size {
        q.push(("size", v.to_string()));
    }
    if let Some(v) = p.reverse {
        q.push(("reverse", v.to_string()));
    }
    q
}
