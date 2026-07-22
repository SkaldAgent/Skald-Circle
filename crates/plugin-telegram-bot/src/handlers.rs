use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::{ChatAction, ParseMode};
use tracing::{error, info};

use core_api::chat_hub::{ModelCommandOutcome, SendMessageOptions};
use core_api::command::expand_template;
use core_api::location::GpsCoord;
use core_api::message_meta::{CommandRef, MessageMetadata};
use core_api::user_channel::UserChannelHandle;

use super::TELEGRAM_FORMAT_CONTEXT;
use super::TgShared;
use super::attachments::TelegramAttachment;
use super::auth::handle_pairing;
use super::events::ensure_forwarder;

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

    // Resolve chat_id → user_id from bindings.
    let user_id = match shared.user_for_chat(chat_id.0).await {
        Some(uid) => uid,
        None => {
            handle_pairing(&bot, chat_id, &shared).await;
            return Ok(());
        }
    };

    // The chat is bound, but access is a separate admin-revocable grant. Gate
    // here so a revoked user is refused immediately, without touching the
    // binding (a re-grant restores service with no re-pairing).
    if !shared.user_authorized(&user_id).await {
        bot.send_message(
            chat_id,
            "⛔ Your access to this bot has been withdrawn by an administrator.",
        )
        .await
        .ok();
        return Ok(());
    }

    // Resolve the user's per-user context (must be unlocked, §9).
    let handle = match shared.user_channel.resolve_user(&user_id).await {
        Some(h) => h,
        None => {
            bot.send_message(
                chat_id,
                "🔒 Your account is locked. Please log in via the web app first, \
                 then send another message.",
            )
            .await
            .ok();
            return Ok(());
        }
    };

    // Ensure a per-user forwarder is running so the response events reach
    // this Telegram chat. The forwarder will exit on broadcast-close (user
    // locked) or plugin stop; a fresh CancellationToken is fine here since
    // the global plugin cancel drops the dispatcher (and thus the shared Arc).
    ensure_forwarder(
        bot.clone(),
        Arc::clone(&shared),
        &user_id,
        chat_id.0,
        Arc::clone(&handle),
        tokio_util::sync::CancellationToken::new(),
    ).await;

    let Some(incoming) = classify_message(&msg) else {
        bot.send_message(chat_id, "Unsupported message format.").await.ok();
        return Ok(());
    };

    let hub = handle.chat_hub();

    match incoming {
        IncomingEvent::Command { ref name, .. } if name == "clear" || name == "new" => {
            handle_clear(&bot, chat_id, &hub).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "sethome" => {
            match hub.set_home("telegram").await {
                Ok(_) => {
                    info!("telegram: set as home source for user {}", user_id);
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
            hub.cancel("telegram").await;
            info!("telegram: agent cancelled via /stop");
            bot.send_message(chat_id, "⏹ Agent stopped.").await.ok();
        }
        IncomingEvent::Command { ref name, .. } if name == "context" => {
            handle_context(&bot, chat_id, &hub).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "cost" => {
            handle_cost(&bot, chat_id, &hub).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "compact" => {
            handle_compact(&bot, chat_id, &hub).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "resettools" => {
            handle_reset_mcp(&bot, chat_id, &hub).await;
        }
        IncomingEvent::Command { ref name, .. } if name == "models" => {
            handle_list_models(&bot, chat_id, &hub).await;
        }
        IncomingEvent::Command { ref name, ref args, .. } if name == "model" => {
            handle_set_model(&bot, chat_id, args, &hub).await;
        }
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
                handle_llm_message(bot, chat_id, content, Some(metadata), shared, &handle).await;
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
            handle_voice(&bot, chat_id, file_id, &shared, &handle).await;
        }
        IncomingEvent::Attachment(attachment) => {
            handle_attachment(bot, chat_id, attachment, shared, &handle).await;
        }
        _ => {
            let text = match &incoming {
                IncomingEvent::Text(t) => t.clone(),
                IncomingEvent::Command { .. }
                | IncomingEvent::Voice { .. }
                | IncomingEvent::Attachment(_) => unreachable!(),
            };

            // If a clarification question is pending for this chat, treat any
            // text as the answer.
            {
                let mut pq_map = shared.pending_questions.lock().await;
                if let Some(pq) = pq_map.remove(&chat_id.0) {
                    let request_id = pq.request_id;
                    let question_msg_id = pq.message_id;
                    drop(pq_map);
                    hub.resolve_question("telegram", request_id, text.clone()).await;
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

            handle_llm_message(bot, chat_id, text, None, shared, &handle).await;
        }
    }

    Ok(())
}

// ── Command handlers ──────────────────────────────────────────────────────────

async fn handle_clear(bot: &Bot, chat_id: ChatId, hub: &Arc<dyn core_api::chat_hub::ChatHubApi>) {
    match hub.clear("telegram").await {
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

async fn handle_context(bot: &Bot, chat_id: ChatId, hub: &Arc<dyn core_api::chat_hub::ChatHubApi>) {
    match hub.context_info("telegram").await {
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

async fn handle_cost(bot: &Bot, chat_id: ChatId, hub: &Arc<dyn core_api::chat_hub::ChatHubApi>) {
    match hub.cost_info("telegram").await {
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

async fn handle_compact(bot: &Bot, chat_id: ChatId, hub: &Arc<dyn core_api::chat_hub::ChatHubApi>) {
    match hub.force_compact("telegram").await {
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

async fn handle_reset_mcp(bot: &Bot, chat_id: ChatId, hub: &Arc<dyn core_api::chat_hub::ChatHubApi>) {
    match hub.reset_mcp("telegram").await {
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

async fn handle_list_models(bot: &Bot, chat_id: ChatId, hub: &Arc<dyn core_api::chat_hub::ChatHubApi>) {
    let items = hub.list_clients_marked("telegram").await;
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

async fn handle_set_model(bot: &Bot, chat_id: ChatId, args: &[String], hub: &Arc<dyn core_api::chat_hub::ChatHubApi>) {
    let arg = args.first().cloned().unwrap_or_default();
    let outcome = hub.apply_model_command("telegram", &arg).await;
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
    handle:   &Arc<dyn UserChannelHandle>,
) {
    bot.send_chat_action(chat_id, ChatAction::Typing).await.ok();

    let hub = handle.chat_hub();
    let client_name = hub.get_selected_client("telegram").await;
    let opts = SendMessageOptions {
        client_name,
        extra_system_context: Some(TELEGRAM_FORMAT_CONTEXT.to_string()),
        tail_reminder:        Some(super::TELEGRAM_FORMAT_REMINDER.to_string()),
        interface_tools:      super::tools::interface_tools(bot.clone(), chat_id, &*shared.tts).await,
        metadata,
        ..Default::default()
    };

    if let Err(e) = hub.send_message("telegram", &text, opts).await {
        error!(error = %e, "telegram: enqueue error");
    }
}

// ── Voice message → transcribe → LLM ─────────────────────────────────────────

async fn handle_voice(
    bot:     &Bot,
    chat_id: ChatId,
    file_id: String,
    shared:  &Arc<TgShared>,
    handle:  &Arc<dyn UserChannelHandle>,
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
    handle_llm_message(bot.clone(), chat_id, message, None, Arc::clone(shared), handle).await;
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
    handle:     &Arc<dyn UserChannelHandle>,
) {
    if let TelegramAttachment::Location { latitude, longitude, accuracy, is_live } = &attachment {
        let coord = GpsCoord { latitude: *latitude, longitude: *longitude };
        shared.location.update("telegram", coord, *accuracy, *is_live);
    }

    bot.send_chat_action(chat_id, ChatAction::UploadDocument).await.ok();

    let downloaded = match attachment.download(&bot).await {
        Ok(d)  => d,
        Err(e) => {
            error!(error = %e, "telegram: failed to download attachment");
            bot.send_message(chat_id, "⚠️ Could not download the attachment.").await.ok();
            return;
        }
    };

    match downloaded {
        Some((file_name, mimetype, bytes)) => {
            // Persist through the shared upload seam so the file lands in the user's
            // home (`uploads/{session}/…`) with an agent-reachable path — identical
            // to a web upload.
            let att = match handle
                .chat_hub()
                .save_upload("telegram", &file_name, mimetype, &bytes)
                .await
            {
                Ok(a)  => a,
                Err(e) => {
                    error!(error = %e, "telegram: failed to save attachment");
                    bot.send_message(chat_id, "⚠️ Could not save the attachment.").await.ok();
                    return;
                }
            };
            info!(chat_id = chat_id.0, path = %att.path, "telegram: attachment saved, forwarding to LLM");
            let caption = match &attachment {
                TelegramAttachment::Document { caption, .. } => caption.clone(),
                TelegramAttachment::Photo    { caption, .. } => caption.clone(),
                TelegramAttachment::Location { .. }          => None,
            }.unwrap_or_default();
            let metadata = MessageMetadata { attachments: vec![att], ..Default::default() };
            handle_llm_message(bot, chat_id, caption, Some(metadata), shared, handle).await;
        }
        None => {
            let message = attachment.system_info_message(None);
            handle_llm_message(bot, chat_id, message, None, shared, handle).await;
        }
    }
}
