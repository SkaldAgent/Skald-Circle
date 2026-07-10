use crate::{
    client::{parse_no_content, HonchoClient},
    error::Result,
    models::*,
    workspaces::page_query,
};

impl HonchoClient {
    /// Create up to 100 conclusions in a single batch.
    pub async fn add_conclusions(
        &self,
        workspace_id: &str,
        batch: &ConclusionBatchCreate,
    ) -> Result<Vec<Conclusion>> {
        self.post(
            &format!("/v3/workspaces/{workspace_id}/conclusions"),
            batch,
        )
        .await
    }

    /// Convenience: add a single conclusion.
    pub async fn add_conclusion(
        &self,
        workspace_id: &str,
        c: ConclusionCreate,
    ) -> Result<Vec<Conclusion>> {
        self.add_conclusions(
            workspace_id,
            &ConclusionBatchCreate {
                conclusions: vec![c],
            },
        )
        .await
    }

    pub async fn list_conclusions(
        &self,
        workspace_id: &str,
        params: &PageParams,
        filter: &ConclusionGet,
    ) -> Result<Page<Conclusion>> {
        self.post_with_query(
            &format!("/v3/workspaces/{workspace_id}/conclusions/list"),
            &page_query(params),
            filter,
        )
        .await
    }

    /// Semantic search over conclusions.
    pub async fn query_conclusions(
        &self,
        workspace_id: &str,
        query: &ConclusionQuery,
    ) -> Result<Vec<Conclusion>> {
        self.post(
            &format!("/v3/workspaces/{workspace_id}/conclusions/query"),
            query,
        )
        .await
    }

    pub async fn delete_conclusion(
        &self,
        workspace_id: &str,
        conclusion_id: &str,
    ) -> Result<()> {
        let url = self.url(&format!(
            "/v3/workspaces/{workspace_id}/conclusions/{conclusion_id}"
        ));
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&self.token)
            .send()
            .await?;
        parse_no_content(resp).await
    }
}
