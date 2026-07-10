use std::sync::Arc;

use axum::{
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::core::chat_hub::{ModelCommandOutcome, SendMessageOptions};
use crate::core::events::{ClientMessage, ServerEvent};
use crate::core::skald::Skald;
use core_api::command::CommandApi;

#[derive(Deserialize)]
pub struct WsParams {
    source: Option<String>,
}

const WEB_FORMAT_CONTEXT: &str = "\
You are responding in a web chat interface. Use standard Markdown formatting for all responses.\n\
\n\
IMAGES: If image generation is active, you can display images to the user using standard Markdown \
image syntax with the URL. Always set a max-width style to avoid the image taking up the full screen width, \
e.g. <img src=\"URL\" style=\"max-width:480px\">. \
The URL returned by image_generate already points to the correct endpoint — use it as-is. \
Do NOT append \".png\" or any extension to the URL.\n\
\n\
FILES: To let the user look at a file directly, call show_file_to_user(path). Supported: \
Markdown, source code, images (PNG/JPG/GIF/WebP/SVG), PDF, and LaTeX (.tex — auto-compiled \
to PDF server-side). HTML opens in a new browser tab. Prefer this over pasting long file \
contents into chat.";

const HELP_TEXT: &str = "\
**Available commands**\n\n\
**/clear** — start a new conversation\n\
**/new** — alias for /clear\n\
**/models** — list available LLM models, ordered by priority\n\
**/model <N|name|auto>** — select the model for this chat\n\
**/context** — show last turn's token usage\n\
**/cost** — show total spend for this session (USD)\n\
**/compact** — force context compaction\n\
**/resettools** — remove all activated tool groups (MCP + config) from the session\n\
**/sethome** — set web as the destination for agent notifications\n\
**/help** — this message";

/// Builds the `/help` text: the static system-command list plus a dynamically
/// discovered "Custom commands" section (`commands/<name>/`).
fn dynamic_help(skald: &Skald) -> String {
    let mut out = String::from(HELP_TEXT);
    let cmds = skald.command_manager().list_enabled();
    if !cmds.is_empty() {
        out.push_str("\n\n**Custom commands**");
        for c in cmds {
            out.push_str(&format!("\n**/{}** — {}", c.name, c.description));
        }
    }
    out
}

// ── Upgrade ───────────────────────────────────────────────────────────────────

pub async fn handler(
    ws:            WebSocketUpgrade,
    Query(params): Query<WsParams>,
    State(skald):  State<Arc<Skald>>,
) -> impl IntoResponse {
    let source = params.source.unwrap_or_else(|| "web".to_string());
    ws.on_upgrade(move |socket| handle_socket(socket, skald, source))
}

// ── Socket loop ───────────────────────────────────────────────────────────────

async fn handle_socket(mut socket: WebSocket, skald: Arc<Skald>, source: String) {
    let session_handler = match skald.chat_hub().session_handler(&source).await {
        Ok(h)  => h,
        Err(e) => {
            let _ = socket.send(to_msg(&ServerEvent::Error { message: e.to_string() })).await;
            return;
        }
    };

    info!(source, "WebSocket connected");

    let mut rx = skald.chat_hub().events(&source);

    // Tell this (possibly reloaded) client whether a turn is already running for
    // its session, so it can restore the STOP button. Sent after subscribing to
    // `rx`, so a turn that finishes right after still delivers its Done via `rx`.
    let _ = socket.send(to_msg(&ServerEvent::TurnRunning {
        running: session_handler.is_processing(),
    })).await;

    loop {
        tokio::select! {
            // ── Inbound: message from the browser ────────────────────────────
            msg = socket.recv() => {
                let text = match msg {
                    Some(Ok(Message::Text(t)))  => t,
                    Some(Ok(Message::Close(_))) | None => return,
                    _ => continue,
                };

                // ── resume ────────────────────────────────────────────────────
                if is_resume_msg(&text) {
                    info!("web WS: resume requested");
                    let hub = Arc::clone(skald.chat_hub());
                    let src = source.clone();
                    tokio::spawn(async move {
                        if let Err(e) = hub.resume(&src).await {
                            tracing::error!(error = %e, source = %src, "resume failed");
                        }
                    });
                    continue;
                }

                // ── cancel / approval / question (mid-turn controls) ──────────
                if is_cancel_msg(&text) {
                    info!("web WS: cancel requested");
                    session_handler.cancel();
                    session_handler.cancel_pending_approvals().await;
                    session_handler.cancel_pending_questions().await;
                    continue;
                }
                if handle_approval_msg(&text, skald.chat_hub()).await { continue; }
                if handle_question_answer_msg(&text, &session_handler).await { continue; }
                if handle_data_msg(&text, &skald) { continue; }
                if handle_select_client_msg(&text, &source, skald.chat_hub()).await { continue; }

                // ── /sethome ──────────────────────────────────────────────────
                let client_msg: ClientMessage = match serde_json::from_str(&text) {
                    Ok(m)  => m,
                    Err(e) => {
                        let _ = socket.send(to_msg(&ServerEvent::Error {
                            message: format!("invalid message: {e}"),
                        })).await;
                        continue;
                    }
                };

                let cmd = client_msg.content.trim();

                if cmd == "/sethome" {
                    let msg = match skald.chat_hub().set_home(&source).await {
                        Ok(_)  => "🏠 Web impostato come **home**. Le notifiche degli agenti arriveranno qui.".to_string(),
                        Err(e) => format!("⚠️ Errore: {e}"),
                    };
                    let _ = socket.send(to_msg(&ServerEvent::Done {
                        message_id:    0,
                        stack_id:      0,
                        content:       msg,
                        input_tokens:  None,
                        output_tokens: None,
                    })).await;
                    continue;
                }

                if cmd == "/help" {
                    let _ = socket.send(to_msg(&ServerEvent::Done {
                        message_id:    0,
                        stack_id:      0,
                        content:       dynamic_help(&skald),
                        input_tokens:  None,
                        output_tokens: None,
                    })).await;
                    continue;
                }

                if cmd == "/context" {
                    match skald.chat_hub().context_info(&source).await {
                        Ok((input, output)) => {
                            let input_str = input.map_or("?".to_string(), |t| t.to_string());
                            let output_str = output.map_or("?".to_string(), |t| t.to_string());
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       format!("↑{input_str} tok · ↓{output_str} tok"),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Err(e) => {
                            let _ = socket.send(to_msg(&ServerEvent::Error { message: e.to_string() })).await;
                        }
                    }
                    continue;
                }

                if cmd == "/cost" {
                    match skald.chat_hub().cost_info(&source).await {
                        Ok(Some(c)) => {
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       format!("💰 Costo sessione: ${c:.4}"),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Ok(None) => {
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       "💰 Nessun costo registrato per questa sessione.".to_string(),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Err(e) => {
                            let _ = socket.send(to_msg(&ServerEvent::Error { message: e.to_string() })).await;
                        }
                    }
                    continue;
                }

                if cmd == "/compact" {
                    match skald.chat_hub().force_compact(&source).await {
                        Ok(true) => {
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       "✅ Contesto compattato.".to_string(),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Ok(false) => {
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       "⏩ Compaction skipped (no messages to summarize or compaction disabled).".to_string(),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Err(e) => {
                            let _ = socket.send(to_msg(&ServerEvent::Error { message: e.to_string() })).await;
                        }
                    }
                    continue;
                }

                if cmd == "/resettools" {
                    match skald.chat_hub().reset_mcp(&source).await {
                        Ok(()) => {
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       "✅ Activated tool groups removed from the session.".to_string(),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                        }
                        Err(e) => {
                            let _ = socket.send(to_msg(&ServerEvent::Error { message: e.to_string() })).await;
                        }
                    }
                    continue;
                }

                if cmd == "/models" {
                    let items = skald.chat_hub().list_clients_marked(&source).await;
                    let content = format_models_md(&items);
                    let _ = socket.send(to_msg(&ServerEvent::Done {
                        message_id:    0,
                        stack_id:      0,
                        content,
                        input_tokens:  None,
                        output_tokens: None,
                    })).await;
                    continue;
                }

                if let Some(arg) = cmd.strip_prefix("/model").map(str::trim) {
                    let outcome = skald.chat_hub().apply_model_command(&source, arg).await;
                    let content = match outcome {
                        ModelCommandOutcome::Set(name)  => format!("✅ Model set: **{name}**"),
                        ModelCommandOutcome::Cleared    => "✅ Model reset to **auto**.".to_string(),
                        ModelCommandOutcome::Error(msg) => format!("⚠️ {msg}"),
                    };
                    let _ = socket.send(to_msg(&ServerEvent::Done {
                        message_id:    0,
                        stack_id:      0,
                        content,
                        input_tokens:  None,
                        output_tokens: None,
                    })).await;
                    continue;
                }

                // ── Custom slash command? ─────────────────────────────────────
                // A recognised custom `/command` expands its `COMMAND.md` template
                // into a normal user message on the `main` session (fully
                // interactive: the model can then ask questions, iterate, dispatch
                // sub-agents). Any other `/...` is an unknown command and is never
                // forwarded to the LLM — reply with a not-found notice + help.
                let mut command_ref: Option<core_api::message_meta::CommandRef> = None;
                let content: String = if cmd.starts_with('/') {
                    let rest = &cmd[1..];
                    let name = rest.split_whitespace().next().unwrap_or("");
                    let args = rest.strip_prefix(name).map(str::trim).unwrap_or("");
                    match skald.command_manager().resolve(name) {
                        Some(command) => {
                            command_ref = Some(core_api::message_meta::CommandRef {
                                name:    command.name.clone(),
                                display: cmd.to_string(),
                            });
                            skald.command_manager().expand(&command.template, args)
                        }
                        None => {
                            let first = cmd.split_whitespace().next().unwrap_or(cmd);
                            let _ = socket.send(to_msg(&ServerEvent::Done {
                                message_id:    0,
                                stack_id:      0,
                                content:       format!("Unknown command: {first}\n\n{}", dynamic_help(&skald)),
                                input_tokens:  None,
                                output_tokens: None,
                            })).await;
                            continue;
                        }
                    }
                } else {
                    client_msg.content.clone()
                };

                // ── Regular LLM message ───────────────────────────────────────
                // Attachments uploaded beforehand, plus an optional custom-command
                // marker. Persisted on the user turn as MessageMetadata; the
                // [SYSTEM INFO] block the LLM sees is generated on the fly by the
                // MessageBuilder (never stored as text), and the UI renders the
                // command's `display` instead of the expanded `content`.
                let attachments = client_msg.attachments.clone();
                let metadata = (!attachments.is_empty() || command_ref.is_some())
                    .then(|| core_api::message_meta::MessageMetadata {
                        attachments: attachments.clone(),
                        command:     command_ref.clone(),
                    });

                // No echo here: the `UserMessage` event is emitted when the message is
                // actually persisted to history (at turn start, or at a round boundary
                // for messages injected mid-turn). This telnet-style echo is what makes
                // the bubble appear in its correct position; clients never render the
                // message optimistically on send.

                let opts = SendMessageOptions {
                    metadata,
                    // The web dropdown is now a view of backend state; the pinned
                    // client lives in ChatHub.selected_clients[source]. The web
                    // `/model` command and the dropdown both flow through
                    // set_selected_client, which broadcasts ClientSelected.
                    client_name:          skald.chat_hub().get_selected_client(&source).await,
                    extra_system_context: Some(WEB_FORMAT_CONTEXT.to_string()),
                    // SPA-only tool: lets the assistant open a file in the user's
                    // viewer. Injected here (not in the registry) so it exists only
                    // for ws.rs clients (web + mobile), never for the Telegram plugin.
                    interface_tools: vec![
                        crate::core::tools::show_file::make_tool(
                            Arc::clone(skald.chat_hub()),
                            source.clone(),
                        ),
                    ],
                    ..Default::default()
                };
                // send_message only enqueues — the turn runs on ChatHub's per-source
                // consumer — so awaiting inline keeps this WS read loop responsive.
                if let Err(e) = skald.chat_hub().send_message(&source, &content, opts).await {
                    tracing::error!(error = %e, source = %source, "send_message enqueue failed");
                }
            }

            // ── Outbound: event from ChatHub → forward to browser ─────────────
            event = rx.recv() => {
                match event {
                    Ok(ge) => {
                        // Forward events for this connection's source.
                        // ApprovalResolved is forwarded regardless of source so the
                        // copilot can react to approvals resolved from other clients.
                        let forward = ge.source.as_deref() == Some(source.as_str())
                            || matches!(ge.event, ServerEvent::ApprovalResolved { .. });
                        if !forward { continue; }
                        debug!(event_type = ge.event.type_name(), "sending event to client");
                        if socket.send(to_msg(&ge.event)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "web WS: event stream lagged");
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────


fn is_cancel_msg(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["type"].as_str().map(|s| s == "cancel"))
        .unwrap_or(false)
}

fn is_resume_msg(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["type"].as_str().map(|s| s == "resume"))
        .unwrap_or(false)
}

/// Returns true if the message was an approval/rejection (caller should `continue`).
async fn handle_approval_msg(
    text:      &str,
    chat_hub:  &Arc<crate::core::chat_hub::ChatHub>,
) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
    let Some(request_id) = v["request_id"].as_i64() else { return false };
    match v["type"].as_str() {
        Some("approve_write") | Some("approve_tool") => {
            // Optional bypass: `bypass_secs` present → approve + bypass.
            // Value 0 means indefinite (session); any positive value is seconds.
            if let Some(bypass_secs) = v["bypass_secs"].as_u64() {
                let secs = if bypass_secs == 0 { None } else { Some(bypass_secs) };
                chat_hub.approval.approve_with_bypass(request_id, secs).await;
            } else {
                chat_hub.approve(request_id).await;
            }
        }
        Some("reject_write") | Some("reject_tool") => {
            let note = v["note"].as_str().unwrap_or("").to_string();
            chat_hub.reject(request_id, note).await;
        }
        _ => return false,
    };
    true
}

/// Returns true if the message was a question answer (caller should `continue`).
async fn handle_question_answer_msg(
    text:    &str,
    handler: &Arc<crate::core::session::handler::ChatSessionHandler>,
) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
    if v["type"].as_str() != Some("answer_question") { return false }
    let Some(request_id) = v["request_id"].as_i64() else { return false };
    let answer = v["answer"].as_str().unwrap_or("").to_string();
    handler.resolve_question(request_id, answer).await;
    true
}

/// Returns true if the message was a select_client event from the web dropdown
/// (caller should `continue`). Mutates the backend's per-source pinned client
/// via `set_selected_client`, which broadcasts `ClientSelected` to every client
/// of the source (so all open tabs/mobile update).
async fn handle_select_client_msg(
    text:     &str,
    source:   &str,
    chat_hub: &Arc<crate::core::chat_hub::ChatHub>,
) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
    if v["type"].as_str() != Some("select_client") { return false }
    let Some(client) = v["client"].as_str() else { return false };
    let client = client.to_string();
    if client == "auto" {
        chat_hub.clear_selected_client(source).await;
    } else {
        chat_hub.set_selected_client(source, client).await;
    }
    true
}

/// Returns true if the message was an inbound data push (caller should `continue`).
/// Dispatches `{"type":"data","stream":"...","payload":{...}}` to the appropriate manager.
fn handle_data_msg(text: &str, skald: &Arc<Skald>) -> bool {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
    if v["type"].as_str() != Some("data") { return false }

    let Ok(msg) = serde_json::from_value::<crate::core::events::InboundDataMessage>(v) else {
        return true;
    };

    match msg.stream.as_str() {
        "location" => {
            let lat = msg.payload["lat"].as_f64();
            let lng = msg.payload["lng"].as_f64();
            let acc = msg.payload["accuracy"].as_f64();
            let live = msg.payload["is_live"].as_bool().unwrap_or(true);
            if let (Some(lat), Some(lng)) = (lat, lng) {
                skald.location_manager().update(
                    "remote",
                    crate::core::location::GpsCoord { latitude: lat, longitude: lng },
                    acc,
                    live,
                );
                tracing::debug!(lat, lng, "location updated from remote client");
            } else {
                tracing::warn!(stream = "location", "missing lat/lng in payload");
            }
        }
        other => tracing::warn!(stream = other, "unknown data stream, ignoring"),
    }

    true
}

fn to_msg(event: &ServerEvent) -> Message {
    Message::Text(event.to_json().into())
}

// ── /models formatter (Markdown, web-specific) ───────────────────────────────
//
// Business logic for `/model` lives in `ChatHub::apply_model_command`; the
// `/models` listing uses `ChatHub::list_clients_marked` and only needs
// rendering. A future `/reasonings` can mirror this thin formatter.

fn format_models_md(items: &[(usize, String, bool)]) -> String {
    let mut text = String::from("**Available models**\n\n");
    for (i, name, is_current) in items {
        let marker = if *is_current { "●" } else { "○" };
        text.push_str(&format!("{marker} `{i:2}`  {name}\n"));
    }
    text.push_str("\nUse `/model N`, `/model name`, or `/model auto`.");
    text
}
