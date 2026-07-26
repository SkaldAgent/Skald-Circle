//! The `LoopEvent → ServerEvent` translator (blueprint §10): ONE subscriber of
//! the loop manager's bus, forwarding to the session's WS channel with the
//! host enrichments the frontend expects (display meta, diff previews, file
//! changes). Byte-parity with the old `TurnEmitter` sequence is the contract.

use std::sync::Arc;

use agent_loop::events::{DeltaKind, Event, LoopEvent};
use agent_loop::store::{CallOutcome, HistoryStore};
use core_api::message_meta::MessageMetadata;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::events::{ServerEvent, TokenDeltaKind};
use crate::mcp::McpProvider;
use crate::tools::{ToolRegistry, is_file_write_tool};

/// Forwards one conversation's loop events to the session's WS `tx`.
pub struct EventTranslator {
    tx:    mpsc::Sender<ServerEvent>,
    tools: Arc<ToolRegistry>,
    mcp:   Arc<dyn McpProvider>,
    store: Arc<dyn HistoryStore>,
    shared: Arc<std::sync::Mutex<TranslateShared>>,
}

/// Turn state the wiring reads back after join (ChatEvent publication).
#[derive(Default)]
pub struct TranslateShared {
    /// The user message id that opened the turn.
    pub user_message_id: Option<i64>,
    /// Accumulated tool calls of the turn (done/failed only — mirrors the old
    /// `all_tool_calls` accumulate rules).
    pub tool_calls:      Vec<core_api::bus::ToolCallEvent>,
}

impl EventTranslator {
    pub fn new(
        tx:    mpsc::Sender<ServerEvent>,
        tools: Arc<ToolRegistry>,
        mcp:   Arc<dyn McpProvider>,
        store: Arc<dyn HistoryStore>,
    ) -> (Self, Arc<std::sync::Mutex<TranslateShared>>) {
        let shared = Arc::new(std::sync::Mutex::new(TranslateShared::default()));
        (Self { tx, tools, mcp, store, shared: shared.clone() }, shared)
    }

    /// Subscribe and forward until `stop` is cancelled (the turn's end).
    pub fn spawn(self, mut rx: tokio::sync::broadcast::Receiver<Event<LoopEvent>>, stop: tokio_util::sync::CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    ev = rx.recv() => {
                        match ev {
                            Ok(ev) => self.forward(ev).await,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!(skipped = n, "event translator lagged; some events were dropped");
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        })
    }

    async fn emit(&self, ev: ServerEvent) {
        self.tx.send(ev).await.ok();
    }

    pub async fn forward(&self, ev: Event<LoopEvent>) {
        let is_root = ev.parent_frame.is_none();
        match ev.inner {
            LoopEvent::TurnStarted | LoopEvent::RoundStarted { .. } | LoopEvent::AsyncResultReady { .. } => {}

            LoopEvent::UserMessage { message_id, content, synthetic, metadata } => {
                // The turn-opening user message (root, non-synthetic) is
                // recorded for the wiring's ChatEvent publication.
                if is_root && !synthetic {
                    let mut g = self.shared.lock().unwrap();
                    if g.user_message_id.is_none() {
                        g.user_message_id = Some(message_id.get());
                    }
                }
                if synthetic {
                    return;
                }
                let meta: Option<MessageMetadata> = metadata
                    .as_ref()
                    .and_then(|v| serde_json::from_value(v.clone()).ok());
                let attachments = meta.as_ref().map(|m| m.attachments.clone()).unwrap_or_default();
                // A custom slash command persists its expanded template (for
                // LLM replay) but the bubble shows the typed command.
                let echo = meta
                    .and_then(|m| m.command.map(|c| c.display))
                    .unwrap_or(content);
                self.emit(ServerEvent::UserMessage { message_id: message_id.get(), content: echo, attachments }).await;
            }

            LoopEvent::TokenDelta { kind, text } => {
                let kind = match kind {
                    DeltaKind::Content   => TokenDeltaKind::Content,
                    DeltaKind::Reasoning => TokenDeltaKind::Reasoning,
                };
                self.emit(ServerEvent::TokenDelta { kind, delta: text }).await;
            }

            LoopEvent::Thinking { message_id, content, usage, reasoning } => {
                self.emit(ServerEvent::Thinking {
                    message_id: message_id.get(),
                    content,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    reasoning_content: reasoning,
                }).await;
            }

            LoopEvent::Done { message_id, content, usage, reasoning } => {
                if !is_root {
                    return; // a child's completion rides AgentFinished
                }
                self.emit(ServerEvent::Done {
                    message_id: message_id.get(),
                    stack_id: ev.frame.get(),
                    content,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    reasoning_content: reasoning,
                }).await;
            }

            LoopEvent::Truncated { output_tokens } => {
                if is_root {
                    self.emit(ServerEvent::Truncated { output_tokens }).await;
                }
            }

            LoopEvent::ToolCallStarted { id, message_id, name, args } => {
                let (display_name, icon) = self.ui_meta(&name, &args);
                let label_short = self.tools.describe_call(&name, &args, core_api::tool::ToolDescriptionLength::Short);
                let label_full  = self.tools.describe_call(&name, &args, core_api::tool::ToolDescriptionLength::Full);
                let path = self.tools.target_path(&name, &args);
                self.emit(ServerEvent::ToolStart {
                    tool_call_id: id.get(),
                    message_id: message_id.get(),
                    name,
                    arguments: args,
                    display_name,
                    icon,
                    label_short,
                    label_full,
                    path,
                }).await;
            }

            LoopEvent::ToolCallFinished { id, outcome } => match outcome {
                CallOutcome::Completed(out) => {
                    let stored = self.store.get_call(id).await.ok().flatten();
                    if let Some(c) = stored.as_ref() {
                        self.shared.lock().unwrap().tool_calls.push(core_api::bus::ToolCallEvent {
                            name:      c.name.clone(),
                            arguments: Some(serde_json::to_string(&c.arguments).unwrap_or_default()),
                            result:    Some(out.to_wire()),
                            status:    "done".to_string(),
                        });
                    }
                    let (preview_old, preview_new) = stored
                        .as_ref()
                        .map(|c| (
                            c.extras["preview_old"].as_str().map(str::to_string),
                            c.extras["preview_new"].as_str().map(str::to_string),
                        ))
                        .unwrap_or((None, None));
                    self.emit(ServerEvent::ToolDone {
                        tool_call_id: id.get(),
                        result: out.to_wire(),
                        result_type: out.kind().to_string(),
                        preview_old,
                        preview_new,
                    }).await;
                    // A successful file-write asks clients holding the file to reload.
                    if let Some(c) = stored
                        && is_file_write_tool(&c.name)
                        && let Some(p) = c.arguments["path"].as_str()
                    {
                        self.emit(ServerEvent::FileChanged { path: crate::approval::normalize_path(p) }).await;
                    }
                }
                CallOutcome::Failed(error) => {
                    let stored = self.store.get_call(id).await.ok().flatten();
                    if let Some(c) = stored.as_ref() {
                        self.shared.lock().unwrap().tool_calls.push(core_api::bus::ToolCallEvent {
                            name:      c.name.clone(),
                            arguments: Some(serde_json::to_string(&c.arguments).unwrap_or_default()),
                            result:    Some(error.clone()),
                            status:    "failed".to_string(),
                        });
                    }
                    self.emit(ServerEvent::ToolError { tool_call_id: id.get(), error }).await;
                }
                CallOutcome::Cancelled => {
                    self.emit(ServerEvent::ToolCancelled { tool_call_id: id.get() }).await;
                }
                CallOutcome::Rejected { reason } => {
                    self.emit(ServerEvent::ToolRejected { tool_call_id: id.get(), reason }).await;
                }
            },

            LoopEvent::ApprovalRequired { id, name, args, request_id } => {
                self.emit(ServerEvent::ApprovalRequired {
                    request_id,
                    tool_call_id: id.get(),
                    tool_name: name,
                    arguments: args,
                }).await;
            }

            LoopEvent::AgentSpawned { frame, agent, depth, prompt_preview, parent_call, parent_agent } => {
                self.emit(ServerEvent::AgentStart {
                    stack_id: frame.get(),
                    parent_tool_call_id: parent_call.get(),
                    agent_id: agent,
                    parent_agent_id: parent_agent,
                    depth: depth as i64,
                    prompt_preview,
                }).await;
            }

            LoopEvent::AgentFinished { frame, agent, result_preview, parent_agent } => {
                self.emit(ServerEvent::AgentDone {
                    stack_id: frame.get(),
                    agent_id: agent,
                    parent_agent_id: parent_agent,
                    result_preview,
                }).await;
            }

            LoopEvent::ModelFallback { from, to, reason } => {
                self.emit(ServerEvent::ModelFallback { from, to, reason: first_line(&reason) }).await;
            }

            LoopEvent::LlmFailed { tried, last_error } => {
                self.emit(ServerEvent::LlmFailed { tried, last_error }).await;
            }

            LoopEvent::Compacted { .. } => {}

            LoopEvent::Error(message) => {
                self.emit(ServerEvent::Error { message }).await;
            }

            LoopEvent::Cancelled => {
                if is_root {
                    self.emit(ServerEvent::Error { message: "Cancelled by user.".to_string() }).await;
                }
            }

            LoopEvent::Host(v) => self.forward_host(v).await,
        }
    }

    /// Host-escaped events (blueprint §4.9): `pending_write` from the
    /// approval gate, `agent_question` from the human channel.
    async fn forward_host(&self, v: Value) {
        match v["type"].as_str() {
            Some("pending_write") => {
                self.emit(ServerEvent::PendingWrite {
                    request_id:   v["request_id"].as_i64().unwrap_or_default(),
                    tool_call_id: v["tool_call_id"].as_i64().unwrap_or_default(),
                    path:         v["path"].as_str().unwrap_or_default().to_string(),
                    old_content:  v["old_content"].as_str().map(str::to_string),
                    new_content:  v["new_content"].as_str().unwrap_or_default().to_string(),
                }).await;
            }
            Some("agent_question") => {
                self.emit(ServerEvent::AgentQuestion {
                    request_id:        v["request_id"].as_i64().unwrap_or_default(),
                    tool_call_id:      v["tool_call_id"].as_i64().unwrap_or_default(),
                    title:             v["title"].as_str().unwrap_or_default().to_string(),
                    question:          v["question"].as_str().unwrap_or_default().to_string(),
                    suggested_answers: v["suggested_answers"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                        .unwrap_or_default(),
                }).await;
            }
            _ => {}
        }
    }

    /// `(display_name, icon)` for a tool card, with the MCP friendly-name
    /// override (mirrors `tool_ui_meta`).
    fn ui_meta(&self, name: &str, args: &Value) -> (String, String) {
        let mut meta = self.tools.display_meta(name, args);
        if let Some((server, tool)) = crate::mcp::parse_mcp_tool_name(name)
            && let Some(friendly) = self.mcp.tool_display_name(server, tool)
        {
            meta.display_name = friendly;
        }
        (meta.display_name, meta.icon)
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).to_string()
}
