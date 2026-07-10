use crate::{
    client::HonchoClient,
    error::Result,
    models::*,
    workspaces::page_query,
};

impl HonchoClient {
    /// Add one or more messages to a session in a single request.
    pub async fn add_messages(
        &self,
        workspace_id: &str,
        session_id: &str,
        batch: &MessageBatchCreate,
    ) -> Result<Vec<Message>> {
        self.post(
            &format!("/v3/workspaces/{workspace_id}/sessions/{session_id}/messages"),
            batch,
        )
        .await
    }

    /// Convenience: add a single message.
    pub async fn add_message(
        &self,
        workspace_id: &str,
        session_id: &str,
        msg: MessageCreate,
    ) -> Result<Vec<Message>> {
        self.add_messages(
            workspace_id,
            session_id,
            &MessageBatchCreate { messages: vec![msg] },
        )
        .await
    }

    pub async fn list_messages(
        &self,
        workspace_id: &str,
        session_id: &str,
        params: &PageParams,
        filter: &MessageGet,
    ) -> Result<Page<Message>> {
        self.post_with_query(
            &format!("/v3/workspaces/{workspace_id}/sessions/{session_id}/messages/list"),
            &page_query(params),
            filter,
        )
        .await
    }

    pub async fn get_message(
        &self,
        workspace_id: &str,
        session_id: &str,
        message_id: &str,
    ) -> Result<Message> {
        self.get(&format!(
            "/v3/workspaces/{workspace_id}/sessions/{session_id}/messages/{message_id}"
        ))
        .await
    }

    pub async fn update_message(
        &self,
        workspace_id: &str,
        session_id: &str,
        message_id: &str,
        body: &MessageUpdate,
    ) -> Result<Message> {
        self.put(
            &format!(
                "/v3/workspaces/{workspace_id}/sessions/{session_id}/messages/{message_id}"
            ),
            body,
        )
        .await
    }
}
