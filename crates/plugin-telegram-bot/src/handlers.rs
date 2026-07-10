use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::{ChatAction, ParseMode};
use tracing::{error, info};

use core_api::chat_hub::{ModelCommandOutcome, SendMessageOptions};
use core_api::command::expand_template;
use core_api::location::GpsCoord;
use core_api::message_meta::{CommandRef, MessageMetadata};

use super::TELEGRAM_FORMAT_CONTEXT;
use super::TgShared;
use super::attachments::TelegramAttachment;
use super::auth::{handle_pairing, load_wl};

// ── Available commands help text (shared by /help and unknown-command replies) ──
const HELP_TEXT: &str = "<b>Available commands</b>\n\n\
     /clear — start a new conversation\n\
     /new — alias for /clear\n\
     /stop — interrupt the agent mid-turn\n\
     /models — list available LLM models, ordered by priority\n\
     /model &lt;N|name|auto&gt; — select the model for this chat\n\
     /context — show last turn's token usage\n\
     /cost — show total spend for this session (USD)\n\
     /compact — force context compaction\n\
     /resettools — remove all activated tool groups (MCP + config) from the session\n\
     /sethome — receive agent notifications here\n\
     /help — this message";

/// Builds the `/help` text: the static system-command list plus a dynamically
/// discovered "Custom commands" section (`commands/<name>/`). Descriptions are
/// HTML-escaped since the message is sent with `ParseMode::Html`.
fn help_text(command: &dyn core_api::command::CommandApi) -> String {
    let mut out = String::from(HELP_TEXT);
    let cmds = command.list_enabled();
    if !cmds.is_empty() {
        out.push_str("\n\n<b>Custom commands</b>");
        for c in cmds {
            out.push_str(&format!(
                "\n/{} — {}",
                c.name,
                super::helpers::escape_html(&c.description)
            ));
        }
    }
    out
}

// ── Incoming message classification ───────────────────────────────────────────
//
// To add a new media type: add a variant to IncomingEvent, handle it in
// classify_message, then dispatch it in message_handler.

pub(crate) enum IncomingEvent {
    Text(String),
    Command { name: String, args: Vec<String> },
    Voice { file_id: String },
    Attachment(TelegramAttachment),
}

pub(crate) fn classify_message(msg: &Message) -> Option<IncomingEvent> {
    if let Some(voice) = msg.voice() {
        return Some(IncomingEvent::Voice { file_id: voice.file.id.to_string() });
    }

    if let Some(doc) = msg.document() {
        return Some(IncomingEvent::Attachment(TelegramAttachment::Document {
            file_id:   doc.file.id.to_string(),
            file_name: doc.file_name.clone().unwrap_or_else(|| "attachment".to_string()),
            mime_type: doc.mime_type.as_ref().map(|m| m.to_string()),
            caption:   msg.caption().map(str::to_string),
        }));
    }

    if let Some(photos) = msg.photo() {
        if let Some(largest) = photos.last() {
            return Some(IncomingEvent::Attachment(TelegramAttachment::Photo {
                file_id: largest.file.id.to_string(),
                caption: msg.caption().map(str::to_string),
            }));
        }
    }

    if let Some(loc) = msg.location() {
        return Some(IncomingEvent::Attachment(TelegramAttachment::Location {
            latitude:  loc.latitude,
            longitude: loc.longitude,
            accuracy:  loc.horizontal_accuracy,
            is_live:   loc.live_period.is_some(),
        }));
    }

    let text = msg.text()?;

    // A command is any message that *starts* with '/'. We deliberately do NOT
    // rely on teloxide's BotCommand entities: those are emitted for every
    // "/token" anywhere in the text, so a normal sentence containing a "/path"
    // (e.g. "stop /usr/bin/foo") would be misclassified as a command. A leading
    // slash is the only signal. Arguments are parsed from the message `text`
    // (not `entity.text()`, which spans only "/model" and would drop the arg).
    // An unknown command is handled by the dispatcher, which replies with help.
    if text.starts_with('/') {
        let full = text.trim_start_matches('/');
        let mut parts = full.splitn(2, ' ');
        let name = parts.next().unwrap_or("").to_ascii_lowercase();
        let name = name.split('@').next().unwrap_or(&name).to_string();
        let rest = parts.next().unwrap_or("").trim().to_string();
        let args: Vec<String> = if rest.is_empty() {
            vec![]
        } else {
            rest.split_whitespace().map(str::to_string).collect()
        };
        return Some(IncomingEvent::Command { name, args });
    }

    Some(IncomingEvent::Text(text.to_string()))
}

// ── Message handler ───────────────────────────────────────────────────────────

pub(crate) async fn message_handler(
    bot:    Bot,
    msg:    Message,
    shared: Arc<TgShared>,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;

    // Whitelist check — re-read the file on every message so agent edits are
    // picked up without a plugin restart.
    let wl = load_wl(&shared.secrets_dir).await;
    if !wl.whitelist.contains(&chat_id.0) {
        handle_pairing(&bot, chat_id, &shared).await;
        return Ok(());
    }

    // Track the last active chat_id so the persistent forwarder knows
    // where to send background notifications.
    *shared.home_chat_id.lock().await = Some(chat_id);

    let Some(incoming) = classify_message(&msg) else {
        bot.send_message(chat_id, "Unsupported message format.").await.ok();
        return Ok(());
    };

    match incoming {
        IncomingEvent::Command { ref name, .. } if name == "clear" || name == "new" => {
            handle_clear(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "sethome" => {
            match shared.chat_hub.set_home("telegram").await {
                Ok(_) => {
                    info!("telegram: set as home source");
                    bot.send_message(chat_id, "🏠 Telegram set as <b>home</b>. Agent notifications will be delivered here.")
                        .parse_mode(ParseMode::Html)
                        .await
                        .ok();
                }
                Err(e) => {
                    bot.send_message(chat_id, format!("⚠️ Error: {e}")).await.ok();
                }
            }
        }
        IncomingEvent::Command { ref name, .. } if name == "help" => {
            bot.send_message(chat_id, help_text(&*shared.command))
                .parse_mode(ParseMode::Html)
                .await
                .ok();
        }
        IncomingEvent::Command { ref name, .. } if name == "stop" => {
            handle_stop(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "context" => {
            handle_context(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "cost" => {
            handle_cost(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "compact" => {
            handle_compact(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "resettools" => {
            handle_reset_mcp(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "models" => {
            handle_list_models(&bot, chat_id, &shared).await;
        }
        IncomingEvent::Command { ref name, ref args, .. } if name == "model" => {
            handle_set_model(&bot, chat_id, args, &shared).await;
        }
        // A recognised custom `/command` expands its `COMMAND.md` template into a
        // normal user message (fully interactive: the model can then ask questions,
        // iterate, dispatch sub-agents). Any other `/...` is an unknown command and
        // is never forwarded to the LLM — reply with a not-found notice + help.
        IncomingEvent::Command { ref name, ref args, .. } => {
            if let Some(resolved) = shared.command.resolve(name) {
                let args_str = args.join(" ");
                let display = if args_str.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {args_str}")
                };
                let content  = expand_template(&resolved.template, &args_str);
                let metadata = MessageMetadata {
                    command: Some(CommandRef {
                        name:    resolved.name,
                        display,
                    }),
                    ..Default::default()
                };
                handle_llm_message(bot, chat_id, content, Some(metadata), shared).await;
            } else {
                bot.send_message(
                    chat_id,
                    format!("Unknown command: /{name}\n\n{}", help_text(&*shared.command)),
                )
                .parse_mode(ParseMode::Html)
                .await
                .ok();
            }
        }
        IncomingEvent::Voice { file_id } => {
            handle_voice(&bot, chat_id, file_id, &shared).await;
        }
        IncomingEvent::Attachment(attachment) => {
            handle_attachment(bot, chat_id, attachment, shared).await;
        }
        _ => {
            let text = match &incoming {
                IncomingEvent::Text(t) => t.clone(),
                IncomingEvent::Command { .. }
                | IncomingEvent::Voice { .. }
                | IncomingEvent::Attachment(_) => unreachable!(),
            };

            // If a clarification question is pending, treat any text as the answer.
            {
                let mut pq = shared.pending_question.lock().await;
                if let Some(pq_inner) = pq.take() {
                    let request_id = pq_inner.request_id;
                    let question_msg_id = pq_inner.message_id;
                    drop(pq);
                    shared.chat_hub.resolve_question("telegram", request_id, text.clone()).await;
                    tracing::info!(request_id, %text, "telegram: clarification answered via text");
                    bot.edit_message_reply_markup(chat_id, question_msg_id)
                        .reply_markup(teloxide::types::InlineKeyboardMarkup::new(vec![vec![
                            teloxide::types::InlineKeyboardButton::callback(
                                format!("✅ {}", super::helpers::escape_html(&text)),
                                "noop",
                            ),
                        ]]))
                        .await
                        .ok();
                    return Ok(());
                }
            }

            handle_llm_message(bot, chat_id, text, None, shared).await;
        }
    }

    Ok(())
}

// ── /clear command ────────────────────────────────────────────────────────────

async fn handle_clear(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    match shared.chat_hub.clear("telegram").await {
        Ok(_) => {
            info!("telegram: session cleared via /clear");
            bot.send_message(chat_id, "🆕 New conversation started.").await.ok();
        }
        Err(e) => {
            error!(error = %e, "telegram: failed to clear session");
            bot.send_message(chat_id, format!("⚠️ Error: {e}")).await.ok();
        }
    }
}

// ── /context command ──────────────────────────────────────────────────────────

async fn handle_context(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    match shared.chat_hub.context_info("telegram").await {
        Ok((input, output)) => {
            let input_str = input.map_or("?".to_string(), |t| t.to_string());
            let output_str = output.map_or("?".to_string(), |t| t.to_string());
            bot.send_message(
                chat_id,
                format!("<i>↑{input_str} tok · ↓{output_str} tok</i>"),
            )
            .parse_mode(ParseMode::Html)
            .await
            .ok();
        }
        Err(e) => {
            bot.send_message(chat_id, format!("⚠️ Error: {e}")).await.ok();
        }
    }
}

// ── /cost command ─────────────────────────────────────────────────────────────

async fn handle_cost(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    match shared.chat_hub.cost_info("telegram").await {
        Ok(Some(c)) => {
            bot.send_message(chat_id, format!("💰 Session cost: ${c:.4}")).await.ok();
        }
        Ok(None) => {
            bot.send_message(chat_id, "💰 No cost recorded for this session.").await.ok();
        }
        Err(e) => {
            bot.send_message(chat_id, format!("⚠️ Error: {e}")).await.ok();
        }
    }
}

// ── /compact command ──────────────────────────────────────────────────────────

async fn handle_compact(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    match shared.chat_hub.force_compact("telegram").await {
        Ok(true) => {
            info!("telegram: manual compaction succeeded");
            bot.send_message(chat_id, "✅ Context compacted.").await.ok();
        }
        Ok(false) => {
            bot.send_message(chat_id, "⏩ Compaction skipped (no messages to summarise or compaction disabled).").await.ok();
        }
        Err(e) => {
            error!(error = %e, "telegram: manual compaction failed");
            bot.send_message(chat_id, format!("⚠️ Compaction failed: {e}")).await.ok();
        }
    }
}

// ── /resettools command ───────────────────────────────────────────────────────

async fn handle_reset_mcp(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    match shared.chat_hub.reset_mcp("telegram").await {
        Ok(()) => {
            info!("telegram: tool-group grants reset via /resettools");
            bot.send_message(chat_id, "✅ Activated tool groups removed from the session.").await.ok();
        }
        Err(e) => {
            error!(error = %e, "telegram: /resettools failed");
            bot.send_message(chat_id, format!("⚠️ Error: {e}")).await.ok();
        }
    }
}

// ── /stop command ────────────────────────────────────────────────────────────

async fn handle_stop(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    shared.chat_hub.cancel("telegram").await;
    info!("telegram: agent cancelled via /stop");
    bot.send_message(chat_id, "⏹ Agent stopped.").await.ok();
}

// ── /models and /model commands ──────────────────────────────────────────────
//
// Business logic (resolve arg, mutate pin, broadcast) lives in
// `ChatHub::apply_model_command` / `ChatHub::list_clients_marked`. Here we only
// format for Telegram (HTML) and send via the bot — same pattern the web WS
// handler uses with Markdown.

async fn handle_list_models(bot: &Bot, chat_id: ChatId, shared: &Arc<TgShared>) {
    let items = shared.chat_hub.list_clients_marked("telegram").await;
    let mut text = String::from("<b>Available models</b>\n\n");
    for (i, name, is_current) in &items {
        let marker = if *is_current { "●" } else { "○" };
        text.push_str(&format!(
            "{} <code>{:2}</code>  {}\n",
            marker,
            i,
            super::helpers::escape_html(name)
        ));
    }
    text.push_str("\nUse <code>/model N</code>, <code>/model name</code>, or <code>/model auto</code>.");
    bot.send_message(chat_id, text)
        .parse_mode(ParseMode::Html)
        .await
        .ok();
}

async fn handle_set_model(bot: &Bot, chat_id: ChatId, args: &[String], shared: &Arc<TgShared>) {
    let arg = args.first().cloned().unwrap_or_default();
    let outcome = shared.chat_hub.apply_model_command("telegram", &arg).await;
    let text = match outcome {
        ModelCommandOutcome::Set(name)  => format!("✅ Model set: <b>{}</b>", super::helpers::escape_html(&name)),
        ModelCommandOutcome::Cleared    => "✅ Model reset to <b>auto</b>.".to_string(),
        ModelCommandOutcome::Error(msg) => format!("⚠️ {}", super::helpers::escape_html(&msg)),
    };
    bot.send_message(chat_id, text)
        .parse_mode(ParseMode::Html)
        .await
        .ok();
}

// ── LLM dispatch ─────────────────────────────────────────────────────────────

async fn handle_llm_message(
    bot:      Bot,
    chat_id:  ChatId,
    text:     String,
    metadata: Option<MessageMetadata>,
    shared:   Arc<TgShared>,
) {
    bot.send_chat_action(chat_id, ChatAction::Typing).await.ok();

    // The persistent_forwarder (spawned once in start()) is always subscribed
    // to the "telegram" broadcast channel and will pick up all events for this
    // turn — including Done → send to Telegram.  No per-message subscription needed.
    let client_name = shared.chat_hub.get_selected_client("telegram").await;
    let opts = SendMessageOptions {
        client_name,
        extra_system_context: Some(TELEGRAM_FORMAT_CONTEXT.to_string()),
        tail_reminder:        Some(super::TELEGRAM_FORMAT_REMINDER.to_string()),
        interface_tools:      super::tools::interface_tools(bot, chat_id, &*shared.tts).await,
        metadata,
        ..Default::default()
    };

    // send_message only enqueues — the turn runs on ChatHub's per-source consumer —
    // so awaiting inline keeps this message handler responsive.
    if let Err(e) = shared.chat_hub.send_message("telegram", &text, opts).await {
        error!(error = %e, "telegram: enqueue error");
    }
}

// ── Voice message → transcribe → LLM ─────────────────────────────────────────

async fn handle_voice(
    bot:     &Bot,
    chat_id: ChatId,
    file_id: String,
    shared:  &Arc<TgShared>,
) {
    use teloxide::net::Download;

    let transcriber = match shared.transcriber().await {
        Some(t) => t,
        None => {
            bot.send_message(chat_id, "⚠️ Transcription not available (no transcription provider configured).").await.ok();
            return;
        }
    };

    let file = match bot.get_file(teloxide::types::FileId(file_id)).await {
        Ok(f)  => f,
        Err(e) => {
            error!(error = %e, "telegram: get_file failed");
            bot.send_message(chat_id, "⚠️ Could not download audio file.").await.ok();
            return;
        }
    };

    let mut audio_bytes = Vec::new();
    if let Err(e) = bot.download_file(&file.path, &mut audio_bytes).await {
        error!(error = %e, "telegram: download_file failed");
        bot.send_message(chat_id, "⚠️ Audio download failed.").await.ok();
        return;
    }

    bot.send_chat_action(chat_id, ChatAction::Typing).await.ok();

    let text = match transcriber.transcribe(audio_bytes, "ogg").await {
        Ok(t)  => t,
        Err(e) => {
            error!(error = %e, "telegram: transcription failed");
            bot.send_message(chat_id, format!("⚠️ Transcription failed: {e}")).await.ok();
            return;
        }
    };

    info!(chat_id = chat_id.0, "telegram: voice transcribed, forwarding to LLM");
    let message = format!(
        "[TELEGRAM SYSTEM INFO]\n\
         The user sent a voice message. The following is the audio transcript:\n\n\
         {text}"
    );
    handle_llm_message(bot.clone(), chat_id, message, None, Arc::clone(shared)).await;
}

// ── Edited message (live location updates) ────────────────────────────────────

pub(crate) async fn edited_message_handler(
    msg:    Message,
    shared: Arc<TgShared>,
) -> ResponseResult<()> {
    if let Some(loc) = msg.location() {
        let coord = GpsCoord { latitude: loc.latitude, longitude: loc.longitude };
        shared.location.update("telegram", coord, loc.horizontal_accuracy, true);
    }
    Ok(())
}

// ── File / media attachment ───────────────────────────────────────────────────

async fn handle_attachment(
    bot:        Bot,
    chat_id:    ChatId,
    attachment: TelegramAttachment,
    shared:     Arc<TgShared>,
) {
    // Update LocationManager immediately, before any LLM dispatch.
    if let TelegramAttachment::Location { latitude, longitude, accuracy, is_live } = &attachment {
        let coord = GpsCoord { latitude: *latitude, longitude: *longitude };
        shared.location.update("telegram", coord, *accuracy, *is_live);
    }

    bot.send_chat_action(chat_id, ChatAction::UploadDocument).await.ok();

    let saved = match attachment.download_and_save(&bot, &shared.uploads_dir, chat_id.0).await {
        Ok(s)  => s,
        Err(e) => {
            error!(error = %e, "telegram: failed to save attachment");
            bot.send_message(chat_id, "⚠️ Could not save the attachment.").await.ok();
            return;
        }
    };

    match saved {
        // Document / Photo: carry the file as structured metadata (rendered as a
        // chip in the copilot UI; the LLM gets the shared [SYSTEM INFO] block).
        // The caption, if any, becomes the user's text for this turn.
        Some(att) => {
            info!(chat_id = chat_id.0, path = %att.path, "telegram: attachment saved, forwarding to LLM");
            let caption = match &attachment {
                TelegramAttachment::Document { caption, .. } => caption.clone(),
                TelegramAttachment::Photo    { caption, .. } => caption.clone(),
                TelegramAttachment::Location { .. }          => None,
            }.unwrap_or_default();
            let metadata = MessageMetadata { attachments: vec![att], ..Default::default() };
            handle_llm_message(bot, chat_id, caption, Some(metadata), shared).await;
        }
        // Location (no file): keep the textual system-info block.
        None => {
            let message = attachment.system_info_message(None);
            handle_llm_message(bot, chat_id, message, None, shared).await;
        }
    }
}
