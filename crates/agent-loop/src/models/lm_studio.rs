//! LM Studio client — a thin wrapper over [`OpenAiModel`] defaulting to
//! `http://localhost:1234/v1` with no API key. (LM Studio can also be served
//! by a YAML-declared provider; this client is kept for explicit use.)

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::openai::OpenAiModel;
use crate::model::{Model, ModelError, ModelRequest, ModelResponse, NamedModel, StreamDelta};

pub struct LmStudioModel {
    inner: OpenAiModel,
}

impl LmStudioModel {
    /// `base_url` defaults to `http://localhost:1234/v1` if `None`.
    pub fn new(base_url: Option<impl Into<String>>, default_model: impl Into<String>) -> Self {
        let url = base_url
            .map(|u| u.into())
            .unwrap_or_else(|| "http://localhost:1234/v1".to_string());
        Self { inner: OpenAiModel::new(url, "", default_model) }
    }
}

impl NamedModel for LmStudioModel {
    fn default_model(&self) -> &str { self.inner.default_model() }
}

#[async_trait]
impl Model for LmStudioModel {
    /// LM Studio is OpenAI-compatible: everything forwards to the inner
    /// client (its pre-delta buffered retry covers local builds rejecting
    /// `stream_options`).
    async fn complete(
        &self,
        req:    &ModelRequest,
        deltas: Option<mpsc::Sender<StreamDelta>>,
    ) -> Result<ModelResponse, ModelError> {
        self.inner.complete(req, deltas).await
    }

    fn is_retriable(&self, err: &ModelError) -> bool { self.inner.is_retriable(err) }
}
