//! Test utilities: a scripted `FakeModel` + builders for kernel and recovery
//! scenarios. (Blueprint: will move behind a `test-util` feature if the crate
//! is ever published.)

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::model::{
    Model, ModelError, ModelRequest, ModelResponse, NamedModel, StreamDelta, ToolCall, Usage,
};

/// One scripted step: the response (or error) plus optional deltas to emit
/// before returning.
pub struct Step {
    pub result:  Result<ModelResponse, ModelError>,
    pub deltas:  Vec<StreamDelta>,
    /// Never return (cancellation tests).
    pub pending: bool,
}

impl Step {
    pub fn message(content: impl Into<String>) -> Self {
        Self { result: Ok(ModelResponse::message(content)), deltas: Vec::new(), pending: false }
    }

    pub fn message_with_usage(content: impl Into<String>, input: u32, output: u32) -> Self {
        let mut resp = ModelResponse::message(content);
        *resp.usage_mut() = Usage {
            input_tokens:  Some(input),
            output_tokens: Some(output),
            ..Usage::default()
        };
        Self { result: Ok(resp), deltas: Vec::new(), pending: false }
    }

    pub fn tool_calls(content: impl Into<String>, calls: Vec<ToolCall>) -> Self {
        Self { result: Ok(ModelResponse::tool_calls(content, calls)), deltas: Vec::new(), pending: false }
    }

    pub fn error(status: Option<u16>, message: impl Into<String>) -> Self {
        Self { result: Err(ModelError::new(status, message)), deltas: Vec::new(), pending: false }
    }

    /// Never completes — the only way out is cancelling the turn.
    pub fn pending() -> Self {
        Self { result: Ok(ModelResponse::message("")), deltas: Vec::new(), pending: true }
    }

    /// Stream these deltas (in order) before returning the response.
    pub fn with_deltas(mut self, deltas: Vec<StreamDelta>) -> Self {
        self.deltas = deltas;
        self
    }
}

/// A scripted model: pops one [`Step`] per `complete` call, records every
/// request for assertions. Clone the `Arc` around it to inspect afterwards.
pub struct FakeModel {
    script:   Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
    default_model: String,
}

impl FakeModel {
    pub fn new(default_model: impl Into<String>, script: Vec<Step>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            requests: Mutex::new(Vec::new()),
            default_model: default_model.into(),
        }
    }

    /// All requests seen so far (one per attempt, fallback included).
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Steps not yet consumed (assert a script was fully driven).
    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

impl NamedModel for FakeModel {
    fn default_model(&self) -> &str { &self.default_model }
}

#[async_trait]
impl Model for FakeModel {
    async fn complete(
        &self,
        req:    &ModelRequest,
        deltas: Option<mpsc::Sender<StreamDelta>>,
    ) -> Result<ModelResponse, ModelError> {
        self.requests.lock().unwrap().push(req.clone());
        let step = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("FakeModel: script exhausted (request for model {})", req.model));
        if let Some(tx) = deltas {
            for d in step.deltas {
                let _ = tx.try_send(d);
            }
        }
        if step.pending {
            std::future::pending::<()>().await;
        }
        step.result
    }
}

/// A `ModelHandle` over a shared `FakeModel` (tests keep the Arc to inspect
/// `requests()` afterwards).
pub fn handle(fake: &std::sync::Arc<FakeModel>, id: &str) -> crate::model::ModelHandle {
    crate::model::ModelHandle {
        id:    id.to_string(),
        model: fake.clone(),
        info:  crate::model::ModelInfo::default(),
    }
}

/// Build a wire `ToolCall` compactly in tests.
pub fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: id.to_string(), name: name.to_string(), arguments: args }
}
