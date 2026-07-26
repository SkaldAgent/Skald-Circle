//! `HumanChannel` + the shipped `ask_user` tool: synchronous
//! question-to-a-human from inside a tool call.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::events::EventSink;
use crate::ids::ToolCallId;
use crate::store::{CallState, HistoryStore};
use crate::tool::{Tool, ToolCtx, ToolFailure, ToolOutput};

/// A question posed to a human.
#[derive(Debug, Clone)]
pub struct Question {
    pub title:     String,
    pub question:  String,
    pub suggested: Vec<String>,
    /// The tool call asking (for UI correlation).
    pub call:      ToolCallId,
    /// The frame asking (for event tagging).
    pub frame:     crate::ids::FrameId,
}

/// The human channel closed while waiting (WS down, user gone).
#[derive(Debug, Clone, Copy)]
pub struct HumanGone;

impl std::fmt::Display for HumanGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("human channel closed")
    }
}
impl std::error::Error for HumanGone {}

#[async_trait]
pub trait HumanChannel: Send + Sync {
    /// Block until an answer arrives. `Err(HumanGone)` = the channel closed:
    /// the tool returns [`ToolFailure::Suspend`] and the call stays
    /// `AwaitingHuman` for a later resume.
    async fn ask(&self, q: Question, events: &EventSink) -> Result<String, HumanGone>;
}

/// The shipped `ask_user` tool. Marks the call `AwaitingHuman` BEFORE
/// suspending (durability rule: a crash mid-question must be recoverable),
/// then blocks on the channel.
pub struct AskUserTool {
    channel: Arc<dyn HumanChannel>,
    store:   Arc<dyn HistoryStore>,
    name:    String,
}

impl AskUserTool {
    pub fn new(channel: Arc<dyn HumanChannel>, store: Arc<dyn HistoryStore>) -> Self {
        Self { channel, store, name: "ask_user".to_string() }
    }

    /// Register under a legacy name (Skald's `ask_user_clarification`, D11).
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str { &self.name }

    fn definition(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": "Ask the user a clarifying question and wait for the answer.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "title":     { "type": "string", "description": "Short title of the question" },
                        "question":  { "type": "string", "description": "The question to ask" },
                        "suggested": { "type": "array", "items": { "type": "string" },
                                       "description": "Optional suggested answers" },
                        "suggested_answers": { "type": "array", "items": { "type": "string" },
                                       "description": "Optional suggested answers (legacy alias of `suggested`)" }
                    },
                    "required": ["question"]
                }
            }
        })
    }

    async fn call(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolFailure> {
        let suggested = args["suggested"]
            .as_array()
            .or_else(|| args["suggested_answers"].as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let q = Question {
            title:     args["title"].as_str().unwrap_or("Question").to_string(),
            question:  args["question"].as_str().unwrap_or("").to_string(),
            suggested,
            call:      ctx.call_id,
            frame:     ctx.frame,
        };
        // Durability FIRST: the call must survive a crash as AwaitingHuman.
        self.store
            .set_call_state(ctx.call_id, CallState::AwaitingHuman)
            .await
            .map_err(|e| ToolFailure::Failed(format!("ask_user: store error: {e}")))?;

        let events = EventSink::from_extensions(&ctx.extensions)
            .ok_or_else(|| ToolFailure::Failed("ask_user: no EventSink in extensions".into()))?;

        match self.channel.ask(q, &events).await {
            Ok(answer)        => Ok(ToolOutput::Text(answer)),
            Err(HumanGone)    => Err(ToolFailure::Suspend),
        }
    }
}
