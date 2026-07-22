//! Transparent logging wrapper for any [`ChatbotClient`].
//!
//! [`LoggingChatbotClient`] intercepts every `chat_with_tools_raw` call, captures
//! the raw HTTP request/response from the inner provider, persists a **metadata-only**
//! row to `llm_requests` in `system.db` (fire-and-forget), then returns the raw data
//! to the caller so it can write the **payload** to the user's own database.
//!
//! The split keeps conversation content (payloads) behind the user key while
//! metadata (cost, tokens, timing) stays in the admin-readable registry.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tracing::warn;

use crate::db::llm_requests;

use super::{ChatOptions, ChatResponse, ChatbotClient, LlmRawMeta, LlmTurn, Message, StreamDelta};

// ─────────────────────────────────────────────────────────────────────────────

pub struct LoggingChatbotClient {
    inner:      Arc<dyn ChatbotClient>,
    pool:       Arc<SqlitePool>,
    model_name: String,
}

impl LoggingChatbotClient {
    pub fn new(
        inner:      Arc<dyn ChatbotClient>,
        pool:       Arc<SqlitePool>,
        model_name: impl Into<String>,
    ) -> Self {
        Self { inner, pool, model_name: model_name.into() }
    }

    /// Shared logging tail of both raw entry points: writes the metadata-only
    /// row to `system.db` (fire-and-forget), then passes the result through.
    async fn log_and_return(
        &self,
        options:  &ChatOptions,
        duration: Duration,
        result:   anyhow::Result<(LlmTurn, Option<LlmRawMeta>)>,
    ) -> anyhow::Result<(LlmTurn, Option<LlmRawMeta>)> {
        let duration_ms = duration.as_millis() as i64;

        let session_id = options.session_id;
        let stack_id   = options.stack_id;
        let user_id    = options.user_id.clone();
        let request_id = options.request_id.clone();
        let model_name = self.model_name.clone();
        let pool       = Arc::clone(&self.pool);

        match result {
            Ok((turn, meta)) => {
                let (input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens) = match &turn {
                    LlmTurn::Message(r) => (r.input_tokens, r.output_tokens, r.cache_read_tokens, r.cache_creation_tokens),
                    LlmTurn::ToolCalls { input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, .. } =>
                        (*input_tokens, *output_tokens, *cache_read_tokens, *cache_creation_tokens),
                };

                tokio::spawn(async move {
                    if let Err(e) = llm_requests::insert(&pool, llm_requests::LlmRequestRow {
                        request_id,
                        user_id,
                        session_id,
                        stack_id,
                        model_name,
                        error_text:            None,
                        input_tokens:          input_tokens.map(|n| n as i64),
                        output_tokens:         output_tokens.map(|n| n as i64),
                        duration_ms,
                        cache_read_tokens:     cache_read_tokens.map(|n| n as i64),
                        cache_creation_tokens: cache_creation_tokens.map(|n| n as i64),
                    }).await {
                        warn!(error = %e, "llm_requests: failed to insert log row");
                    }
                });

                Ok((turn, meta))
            }

            Err(e) => {
                let error_text = e.to_string();

                tokio::spawn(async move {
                    if let Err(log_err) = llm_requests::insert(&pool, llm_requests::LlmRequestRow {
                        request_id,
                        user_id,
                        session_id,
                        stack_id,
                        model_name,
                        error_text:            Some(error_text),
                        input_tokens:          None,
                        output_tokens:         None,
                        duration_ms,
                        cache_read_tokens:     None,
                        cache_creation_tokens: None,
                    }).await {
                        warn!(error = %log_err, "llm_requests: failed to insert error log row");
                    }
                });

                Err(e)
            }
        }
    }
}

#[async_trait]
impl ChatbotClient for LoggingChatbotClient {
    /// Passthrough — logging only applies to the tool-calling path.
    async fn chat(
        &self,
        messages: &[Message],
        options:  &ChatOptions,
    ) -> anyhow::Result<ChatResponse> {
        self.inner.chat(messages, options).await
    }

    /// Passthrough that drops the raw meta. Used by callers that do not need
    /// payload capture (e.g. the compactor).
    async fn chat_with_tools(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
    ) -> anyhow::Result<LlmTurn> {
        let (turn, _) = self.chat_with_tools_raw(messages, tools, options).await?;
        Ok(turn)
    }

    /// Intercepts the call, delegates to `inner.chat_with_tools_raw` to capture
    /// HTTP wire data, writes a **metadata-only** row to `system.db`, then returns
    /// the raw data so the caller can persist payloads to the user's own database.
    async fn chat_with_tools_raw(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
    ) -> anyhow::Result<(LlmTurn, Option<LlmRawMeta>)> {
        let start  = Instant::now();
        let result = self.inner.chat_with_tools_raw(messages, tools, options).await;
        self.log_and_return(options, start.elapsed(), result).await
    }

    /// Streaming twin of `chat_with_tools_raw`: forwards `delta_tx` untouched to
    /// the inner client (deltas are not logged — only the final turn is), then
    /// applies the same metadata logging. Without this override the trait
    /// default would silently fall back to the buffered call.
    async fn chat_with_tools_raw_streaming(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
        delta_tx: mpsc::Sender<StreamDelta>,
    ) -> anyhow::Result<(LlmTurn, Option<LlmRawMeta>)> {
        let start  = Instant::now();
        let result = self.inner.chat_with_tools_raw_streaming(messages, tools, options, delta_tx).await;
        self.log_and_return(options, start.elapsed(), result).await
    }
}
