use async_trait::async_trait;
use serde_json::Value;

use crate::{ChatOptions, ChatResponse, ChatbotClient, LlmRawMeta, LlmTurn, Message, openai::OpenAiClient};

/// LM Studio client.
///
/// LM Studio exposes an OpenAI-compatible `/v1` endpoint, so this is a thin
/// wrapper that defaults to `http://localhost:1234/v1` and requires no API key.
pub struct LmStudioClient {
    inner: OpenAiClient,
}

impl LmStudioClient {
    /// `base_url` defaults to `http://localhost:1234/v1` if `None`.
    pub fn new(base_url: Option<impl Into<String>>) -> Self {
        let url = base_url
            .map(|u| u.into())
            .unwrap_or_else(|| "http://localhost:1234/v1".to_string());
        Self { inner: OpenAiClient::new(url, "", None, false) }
    }
}

#[async_trait]
impl ChatbotClient for LmStudioClient {
    async fn chat(
        &self,
        messages: &[Message],
        options:  &ChatOptions,
    ) -> anyhow::Result<ChatResponse> {
        self.inner.chat(messages, options).await
    }

    async fn chat_with_tools(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
    ) -> anyhow::Result<LlmTurn> {
        self.inner.chat_with_tools(messages, tools, options).await
    }

    async fn chat_with_tools_raw(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
    ) -> anyhow::Result<(LlmTurn, Option<LlmRawMeta>)> {
        self.inner.chat_with_tools_raw(messages, tools, options).await
    }
}
