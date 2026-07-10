//! Transparent logging wrapper for any [`ChatbotClient`].
//!
//! [`LoggingChatbotClient`] intercepts every `chat_with_tools` call, captures
//! the raw HTTP request/response from the inner provider via `chat_with_tools_raw`,
//! then persists a row to `llm_requests` asynchronously (fire-and-forget).
//!
//! The LLM loop is completely unaware of this: it only holds an
//! `Arc<dyn ChatbotClient>` and calls `chat_with_tools` as usual.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::SqlitePool;
use tracing::warn;

use crate::core::db::llm_requests;

use super::{ChatOptions, ChatResponse, ChatbotClient, LlmRawMeta, LlmTurn, Message};

// ─────────────────────────────────────────────────────────────────────────────

/// Controls which parts of the HTTP exchange are persisted per row.
#[derive(Debug, Clone, Copy)]
pub struct LogSaveFlags {
    pub request_payload:  bool,
    pub response_payload: bool,
    pub request_headers:  bool,
    pub response_headers: bool,
}

impl Default for LogSaveFlags {
    fn default() -> Self {
        Self { request_payload: true, response_payload: true, request_headers: true, response_headers: true }
    }
}

pub struct LoggingChatbotClient {
    inner:      Arc<dyn ChatbotClient>,
    pool:       Arc<SqlitePool>,
    model_name: String,
    flags:      LogSaveFlags,
}

impl LoggingChatbotClient {
    pub fn new(
        inner:      Arc<dyn ChatbotClient>,
        pool:       Arc<SqlitePool>,
        model_name: impl Into<String>,
        flags:      LogSaveFlags,
    ) -> Self {
        Self { inner, pool, model_name: model_name.into(), flags }
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

    /// Intercepts the call, delegates to `inner.chat_with_tools_raw` to capture
    /// HTTP wire data, then spawns a fire-and-forget DB write before returning.
    async fn chat_with_tools(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
    ) -> anyhow::Result<LlmTurn> {
        let start  = Instant::now();
        let result = self.inner.chat_with_tools_raw(messages, tools, options).await;
        let duration_ms = start.elapsed().as_millis() as i64;

        let session_id = options.session_id;
        let stack_id   = options.stack_id;
        let model_name = self.model_name.clone();
        let pool       = Arc::clone(&self.pool);

        match result {
            Ok((turn, meta)) => {
                let (input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens) = match &turn {
                    LlmTurn::Message(r) => (r.input_tokens, r.output_tokens, r.cache_read_tokens, r.cache_creation_tokens),
                    LlmTurn::ToolCalls { input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, .. } =>
                        (*input_tokens, *output_tokens, *cache_read_tokens, *cache_creation_tokens),
                };

                let meta = meta.unwrap_or_default();
                let flags = self.flags;
                let request_json = if flags.request_payload {
                    meta.request_body.map(|v| v.to_string()).unwrap_or_default()
                } else { String::new() };
                let request_headers  = if flags.request_headers  { meta.request_headers.map(|v| v.to_string())  } else { None };
                let response_json    = if flags.response_payload  { meta.response_body.map(|v| v.to_string())   } else { None };
                let response_headers = if flags.response_headers  { meta.response_headers.map(|v| v.to_string()) } else { None };

                tokio::spawn(async move {
                    if let Err(e) = llm_requests::insert(&pool, llm_requests::LlmRequestRow {
                        session_id,
                        stack_id,
                        model_name,
                        request_json,
                        request_headers,
                        response_json,
                        response_headers,
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

                Ok(turn)
            }

            Err(e) => {
                let error_text = e.to_string();

                tokio::spawn(async move {
                    if let Err(log_err) = llm_requests::insert(&pool, llm_requests::LlmRequestRow {
                        session_id,
                        stack_id,
                        model_name,
                        request_json:    String::new(),
                        request_headers: None,
                        response_json:   None,
                        response_headers: None,
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

    /// Expose raw metadata so this wrapper can itself be wrapped if needed.
    async fn chat_with_tools_raw(
        &self,
        messages: &[Value],
        tools:    &[Value],
        options:  &ChatOptions,
    ) -> anyhow::Result<(LlmTurn, Option<LlmRawMeta>)> {
        self.inner.chat_with_tools_raw(messages, tools, options).await
    }
}
