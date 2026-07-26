//! Transparent logging decorator for any [`agent_loop::model::Model`].
//!
//! [`LoggingModel`] intercepts every `complete` call, measures the duration and
//! persists, fire-and-forget:
//!
//! * a **metadata-only** row in `llm_requests` (`system.db`) — cost, tokens,
//!   timing, plus the correlation the UI filters on (`user_id`, `session_id`,
//!   `stack_id`);
//! * the **payload** (request/response bodies + headers) in
//!   `llm_request_payloads` in the caller's own database, keyed by the same
//!   `request_id`.
//!
//! The split keeps conversation content behind the user key while metadata
//! stays in the admin-readable registry (§5.1).
//!
//! **Correlation is the decorator's, not the request's.** The owner
//! ([`RequestLogTarget`]) is captured when the model is handed out — the
//! `ModelSelector` builds one decorator per selection, and it is the only place
//! in the process that knows *whose* traffic this is. Session and frame come
//! from the request itself (`conversation` / `frame`), so every caller — a
//! round of the kernel loop, a sub-agent frame, a compaction summary — is
//! attributed with no extra plumbing. `ModelRequest::log` is therefore unused
//! by this host.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tracing::warn;

use agent_loop::model::{Model, ModelError, ModelRequest, ModelResponse, RawMeta, StreamDelta};

use crate::db::{llm_request_payloads, llm_requests};
use crate::loop_adapters::history::SqliteHistory;

/// Who the logged traffic belongs to.
#[derive(Clone, Default)]
pub struct RequestLogTarget {
    /// Owner of the call — the `user_id` column of the metadata row, and what
    /// the LLM-requests page filters on.
    pub user_id:  Option<String>,
    /// The owner's own (SQLCipher) pool: destination of the payload rows.
    /// `None` disables payload logging, keeping metadata only.
    pub payloads: Option<Arc<SqlitePool>>,
}

impl RequestLogTarget {
    /// A user's traffic: metadata attributed to them, payloads in their pool.
    pub fn user(user_id: impl Into<String>, pool: Arc<SqlitePool>) -> Self {
        Self { user_id: Some(user_id.into()), payloads: Some(pool) }
    }
}

pub struct LoggingModel {
    inner:      Arc<dyn Model>,
    /// `system.db` — the registry the metadata row lands in.
    registry:   Arc<SqlitePool>,
    model_name: String,
    target:     RequestLogTarget,
}

impl LoggingModel {
    pub fn new(
        inner:      Arc<dyn Model>,
        registry:   Arc<SqlitePool>,
        model_name: impl Into<String>,
        target:     RequestLogTarget,
    ) -> Self {
        Self { inner, registry, model_name: model_name.into(), target }
    }

    /// Persists the request/response bodies in the owner's own database.
    /// Fire-and-forget: a failed write must never break the turn.
    fn spawn_payload(&self, request_id: &str, raw: &RawMeta) {
        let Some(pool) = self.target.payloads.clone() else { return };
        let row = llm_request_payloads::PayloadRow {
            request_id:       request_id.to_string(),
            request_json:     raw.request_body.as_ref().map(|v| v.to_string()).unwrap_or_default(),
            request_headers:  raw.request_headers.as_ref().map(|v| v.to_string()),
            response_json:    raw.response_body.as_ref().map(|v| v.to_string()),
            response_headers: raw.response_headers.as_ref().map(|v| v.to_string()),
        };
        tokio::spawn(async move {
            if let Err(e) = llm_request_payloads::insert(&pool, row).await {
                warn!(error = %e, "llm_request_payloads: failed to insert");
            }
        });
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

        // Correlation: the owner is ours, the conversation/frame are the call's.
        let session_id = SqliteHistory::session_id(&req.conversation).ok();
        let stack_id   = Some(req.frame.0);
        let user_id    = self.target.user_id.clone();
        let request_id = Some(req.request_id.clone());
        let model_name = self.model_name.clone();
        let pool       = Arc::clone(&self.registry);

        match &result {
            Ok(resp) => {
                let usage = resp.usage();
                let (input_tokens, output_tokens, cache_read, cache_write) = (
                    usage.input_tokens.map(|n| n as i64),
                    usage.output_tokens.map(|n| n as i64),
                    usage.cache_read.map(|n| n as i64),
                    usage.cache_write.map(|n| n as i64),
                );
                if let Some(raw) = resp.raw() {
                    self.spawn_payload(&req.request_id, raw);
                }
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
                // Only an HTTP failure carries a body (a provider 400 is exactly
                // what the debug page is for); network/parse errors carry none.
                if let Some(raw) = e.raw.as_ref() {
                    self.spawn_payload(&req.request_id, raw);
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_loop::ids::{ConversationId, FrameId};
    use agent_loop::model::Usage;
    use agent_loop::testing::{FakeModel, Step};
    use serde_json::{Value, json};

    fn temp_db_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        p.push(format!("skald-test-{tag}-{}-{nanos}.db", std::process::id()));
        p.to_string_lossy().into_owned()
    }

    fn cleanup(path: &str) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path}{suffix}"));
        }
    }

    fn raw() -> RawMeta {
        RawMeta {
            request_headers:  Some(json!({ "authorization": "REDACTED" })),
            request_body:     Some(json!({ "model": "m", "messages": [] })),
            response_headers: Some(json!({ "content-type": "application/json" })),
            response_body:    Some(json!({ "choices": [] })),
        }
    }

    fn request(request_id: &str, session_id: i64, stack_id: i64) -> ModelRequest {
        ModelRequest {
            messages:     Vec::new(),
            tools:        Vec::new(),
            model:        "m".into(),
            max_tokens:   None,
            temperature:  None,
            request_id:   request_id.into(),
            conversation: ConversationId::new(format!("session:{session_id}")),
            frame:        FrameId(stack_id),
            extras:       Value::Null,
            log:          None,
        }
    }

    /// The rows are written fire-and-forget from a spawned task.
    async fn wait_for(pool: &SqlitePool, sql: &'static str) -> i64 {
        for _ in 0..100 {
            let n = sqlx::query_scalar::<_, i64>(sql).fetch_one(pool).await.unwrap();
            if n > 0 {
                return n;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("no row appeared for: {sql}");
    }

    /// The regression that made the LLM-requests page empty after the
    /// agent-loop migration: a row written with no `user_id` (the page filters
    /// on it) and no payload.
    #[tokio::test]
    async fn logs_metadata_with_owner_and_payload() {
        let path = temp_db_path("llmlog-ok");
        let pool = Arc::new(crate::db::init_system_pool(&path).await.unwrap());

        let mut resp = ModelResponse::message("hi");
        *resp.usage_mut() = Usage {
            input_tokens:  Some(11),
            output_tokens: Some(7),
            ..Usage::default()
        };
        let ModelResponse::Message { content, reasoning, usage, .. } = resp else { unreachable!() };
        let scripted = ModelResponse::Message { content, reasoning, usage, raw: Some(raw()) };

        let inner = Arc::new(FakeModel::new("m", vec![Step { result: Ok(scripted), deltas: Vec::new(), pending: false }]));
        let model = LoggingModel::new(
            inner,
            Arc::clone(&pool),
            "gpt-test",
            RequestLogTarget::user("u-1", Arc::clone(&pool)),
        );

        model.complete(&request("req-1", 42, 7), None).await.unwrap();

        wait_for(&pool, "SELECT COUNT(*) FROM llm_requests").await;
        let (user_id, session_id, stack_id, model_name, input, output): (Option<String>, Option<i64>, Option<i64>, String, Option<i64>, Option<i64>) =
            sqlx::query_as("SELECT user_id, session_id, stack_id, model_name, input_tokens, output_tokens
                            FROM llm_requests WHERE request_id = 'req-1'")
                .fetch_one(&*pool).await.unwrap();
        assert_eq!(user_id.as_deref(), Some("u-1"), "the page filters on user_id");
        assert_eq!(session_id, Some(42));
        assert_eq!(stack_id, Some(7));
        assert_eq!(model_name, "gpt-test");
        assert_eq!((input, output), (Some(11), Some(7)));

        wait_for(&pool, "SELECT COUNT(*) FROM llm_request_payloads").await;
        let body: String = sqlx::query_scalar(
            "SELECT request_json FROM llm_request_payloads WHERE request_id = 'req-1'")
            .fetch_one(&*pool).await.unwrap();
        assert!(body.contains("\"messages\""), "payload not persisted: {body}");

        pool.close().await;
        cleanup(&path);
    }

    /// A failed call is logged too, with the provider's rejected body attached.
    #[tokio::test]
    async fn logs_error_row_and_error_payload() {
        let path = temp_db_path("llmlog-err");
        let pool = Arc::new(crate::db::init_system_pool(&path).await.unwrap());

        let err = ModelError::new(Some(400), "bad request").with_raw(raw());
        let inner = Arc::new(FakeModel::new("m", vec![Step { result: Err(err), deltas: Vec::new(), pending: false }]));
        let model = LoggingModel::new(
            inner,
            Arc::clone(&pool),
            "gpt-test",
            RequestLogTarget::user("u-2", Arc::clone(&pool)),
        );

        assert!(model.complete(&request("req-2", 5, 9), None).await.is_err());

        wait_for(&pool, "SELECT COUNT(*) FROM llm_requests").await;
        let (user_id, error_text): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT user_id, error_text FROM llm_requests WHERE request_id = 'req-2'")
                .fetch_one(&*pool).await.unwrap();
        assert_eq!(user_id.as_deref(), Some("u-2"));
        assert!(error_text.unwrap_or_default().contains("bad request"));

        wait_for(&pool, "SELECT COUNT(*) FROM llm_request_payloads").await;

        pool.close().await;
        cleanup(&path);
    }

    /// No target pool ⇒ metadata only (payloads are the owner's, and an owner
    /// with a locked database must not silently lose the metadata row).
    #[tokio::test]
    async fn without_payload_pool_only_metadata_is_written() {
        let path = temp_db_path("llmlog-meta");
        let pool = Arc::new(crate::db::init_system_pool(&path).await.unwrap());

        let inner = Arc::new(FakeModel::new("m", vec![Step::message("hi")]));
        let model = LoggingModel::new(
            inner,
            Arc::clone(&pool),
            "gpt-test",
            RequestLogTarget { user_id: Some("u-3".into()), payloads: None },
        );

        model.complete(&request("req-3", 1, 2), None).await.unwrap();

        wait_for(&pool, "SELECT COUNT(*) FROM llm_requests").await;
        let payloads: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM llm_request_payloads")
            .fetch_one(&*pool).await.unwrap();
        assert_eq!(payloads, 0);

        pool.close().await;
        cleanup(&path);
    }
}
