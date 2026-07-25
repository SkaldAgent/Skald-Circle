//! Transparent logging decorator for any [`agent_loop::model::Model`].
//!
//! [`LoggingModel`] intercepts every `complete` call, measures the duration,
//! and persists a **metadata-only** row to `llm_requests` in `system.db`
//! (fire-and-forget). Per-request correlation (session/stack/user id) travels
//! in [`ModelRequest::log`], set by the caller; the payload (request/response
//! bodies) is returned to the caller inside [`ModelResponse::raw`] /
//! [`ModelError::raw`] so it can be written to the user's own database.
//!
//! The split keeps conversation content (payloads) behind the user key while
//! metadata (cost, tokens, timing) stays in the admin-readable registry.
//! (Successor of `chatbot::logging::LoggingChatbotClient`, blueprint D13.)

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tracing::warn;

use agent_loop::model::{Model, ModelError, ModelRequest, ModelResponse, StreamDelta};

use crate::db::llm_requests;

pub struct LoggingModel {
    inner:      Arc<dyn Model>,
    pool:       Arc<SqlitePool>,
    model_name: String,
}

impl LoggingModel {
    pub fn new(inner: Arc<dyn Model>, pool: Arc<SqlitePool>, model_name: impl Into<String>) -> Self {
        Self { inner, pool, model_name: model_name.into() }
    }
}

#[async_trait]
impl Model for LoggingModel {
    async fn complete(
        &self,
        req:    &ModelRequest,
        deltas: Option<mpsc::Sender<StreamDelta>>,
    ) -> Result<ModelResponse, ModelError> {
        let start  = Instant::now();
        let result = self.inner.complete(req, deltas).await;
        let duration_ms = start.elapsed().as_millis() as i64;

        // Per-request correlation set by the caller (llm_call / compactor).
        let log        = req.log.clone().unwrap_or_default();
        let session_id = log["session_id"].as_i64();
        let stack_id   = log["stack_id"].as_i64();
        let user_id    = log["user_id"].as_str().map(str::to_string);
        let request_id = Some(req.request_id.clone());
        let model_name = self.model_name.clone();
        let pool       = Arc::clone(&self.pool);

        match &result {
            Ok(resp) => {
                let usage = resp.usage();
                let (input_tokens, output_tokens, cache_read, cache_write) = (
                    usage.input_tokens.map(|n| n as i64),
                    usage.output_tokens.map(|n| n as i64),
                    usage.cache_read.map(|n| n as i64),
                    usage.cache_write.map(|n| n as i64),
                );
                tokio::spawn(async move {
                    if let Err(e) = llm_requests::insert(&pool, llm_requests::LlmRequestRow {
                        request_id,
                        user_id,
                        session_id,
                        stack_id,
                        model_name,
                        error_text:            None,
                        input_tokens,
                        output_tokens,
                        duration_ms,
                        cache_read_tokens:     cache_read,
                        cache_creation_tokens: cache_write,
                    }).await {
                        warn!(error = %e, "llm_requests: failed to insert log row");
                    }
                });
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
            }
        }

        result
    }

    fn is_retriable(&self, err: &ModelError) -> bool {
        self.inner.is_retriable(err)
    }
}
