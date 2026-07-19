use std::sync::Arc;

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use core_api::events::{GlobalEvent, ServerEvent};
use core_api::user_channel::UserChannelHandle;

use super::TgShared;
use super::helpers::{escape_html, label_to_html, send_long};

/// Sends an inline keyboard for an approval request and records the request.
async fn send_approval_keyboard(
    bot:        &Bot,
    chat_id:    ChatId,
    text:       String,
    user_id:    String,
    request_id: i64,
    shared:     &Arc<TgShared>,
) {
    let keyboard = InlineKeyboardMarkup::new(vec![
        vec![
            InlineKeyboardButton::callback("✅ Approve",  format!("approve:{request_id}")),
            InlineKeyboardButton::callback("❌ Reject",   format!("reject:{request_id}")),
        ],
        vec![
            InlineKeyboardButton::callback("⏱ 15 min",  format!("bypass_time:900:{request_id}")),
            InlineKeyboardButton::callback("🔄 Session", format!("bypass_session:{request_id}")),
        ],
    ]);

    match bot
        .send_message(chat_id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await
    {
        Ok(m) => {
            shared.pending_approvals.lock().await.insert(
                m.id,
                super::PendingApproval { user_id, request_id },
            );
        }
        Err(e) => error!(error = %e, "telegram: failed to send approval message"),
    }
}

// ── Per-user forwarder ────────────────────────────────────────────────────────

/// Spawns forwarders for all bound users whose contexts are already unlocked.
/// Called at plugin start. Users who log in later get their forwarder spawned
/// lazily on first incoming message.
pub(crate) async fn spawn_forwarders_for_bound_users(
    bot:    &Bot,
    shared: &Arc<TgShared>,
    cancel: &CancellationToken,
) {
    let bindings = shared.bindings.read().await.clone();
    for b in &bindings.bindings {
        // Skip users whose access was revoked — don't spin up a forwarder for
        // a chat the bot will refuse to serve anyway (inbound is gated too).
        if !shared.user_authorized(&b.user_id).await {
            continue;
        }
        if let Some(handle) = shared.user_channel.resolve_user(&b.user_id).await {
            ensure_forwarder(bot.clone(), Arc::clone(shared), &b.user_id, b.chat_id, handle, cancel.clone()).await;
        }
    }
}

/// Spawns a per-user forwarder if one is not already running for `user_id`.
/// The forwarder subscribes to the user's event stream and routes `ServerEvent`s
/// to the bound Telegram `chat_id`.
pub(crate) async fn ensure_forwarder(
    bot:      Bot,
    shared:   Arc<TgShared>,
    user_id:  &str,
    chat_id:  i64,
    handle:   Arc<dyn UserChannelHandle>,
    cancel:   CancellationToken,
) {
    let mut forwarders = shared.forwarders.lock().await;
    if forwarders.contains(user_id) {
        return;
    }
    forwarders.insert(user_id.to_string());

    let uid = user_id.to_string();
    info!(user_id = %uid, chat_id, "telegram: spawning per-user forwarder");

    let shared_c = Arc::clone(&shared);
    let cancel_c = cancel.clone();
    tokio::spawn(user_forwarder(bot, shared_c, uid, chat_id, handle, cancel_c));
}

/// One forwarder per unlocked user. Subscribes to the user's `global_tx` and
/// routes events to Telegram. Exits when the broadcast channel closes (user
/// context dropped at restart / lock) or the plugin is cancelled.
async fn user_forwarder(
    bot:      Bot,
    shared:   Arc<TgShared>,
    user_id:  String,
    chat_id:  i64,
    handle:   Arc<dyn UserChannelHandle>,
    cancel:   CancellationToken,
) {
    let mut rx = handle.subscribe();
    let tg_chat = ChatId(chat_id);

    loop {
        let ge: GlobalEvent = tokio::select! {
            _ = cancel.cancelled() => {
                info!(user_id = %user_id, "telegram: forwarder cancelled");
                break;
            }
            result = rx.recv() => match result {
                Ok(e)                                       => e,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(user_id = %user_id, skipped = n, "telegram: forwarder lagged");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!(user_id = %user_id, "telegram: forwarder — user context closed, exiting");
                    break;
                }
            },
        };

        // ApprovalResolved is handled regardless of source so Telegram removes
        // its keyboard even when the approval was resolved via web or REST.
        if let ServerEvent::ApprovalResolved { request_id, .. } = ge.event {
            let mut pending = shared.pending_approvals.lock().await;
            if let Some((&msg_id, _)) = pending.iter().find(|(_, pa)| pa.request_id == request_id) {
                let msg_id = msg_id;
                pending.remove(&msg_id);
                drop(pending);
                bot.delete_message(tg_chat, msg_id).await.ok();
            }
            continue;
        }

        // Only process events from the "telegram" source.
        if ge.source.as_deref() != Some("telegram") {
            continue;
        }

        let event = ge.event;

        match event {
            ServerEvent::Done { content, .. } => {
                if !content.trim().is_empty() {
                    send_long(&bot, tg_chat, &content, Some(ParseMode::Html)).await;
                }
            }

            ServerEvent::Error { message } => {
                bot.send_message(
                    tg_chat,
                    format!("⚠️ <b>Error:</b> {}", escape_html(&message)),
                )
                .parse_mode(ParseMode::Html)
                .await
                .ok();
            }

            ServerEvent::ToolStart { label_short, .. } => {
                bot.send_message(tg_chat, format!("🔧 <i>{}</i>…", label_to_html(&label_short)))
                    .parse_mode(ParseMode::Html)
                    .await
                    .ok();
            }

            ServerEvent::Thinking { content, .. } => {
                if !content.trim().is_empty() {
                    send_long(&bot, tg_chat, &content, Some(ParseMode::Html)).await;
                }
            }

            ServerEvent::AgentStart { agent_id, parent_agent_id, prompt_preview, .. } => {
                let preview  = prompt_preview.chars().take(300).collect::<String>();
                let ellipsis = if prompt_preview.len() > 300 { "…" } else { "" };
                bot.send_message(
                    tg_chat,
                    format!(
                        "🤖 <b>{}</b> → <b>{}</b>\n<blockquote>{}{ellipsis}</blockquote>",
                        escape_html(&parent_agent_id),
                        escape_html(&agent_id),
                        escape_html(&preview),
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await
                .ok();
            }

            ServerEvent::AgentDone { agent_id, parent_agent_id, result_preview, .. } => {
                let preview  = result_preview.chars().take(300).collect::<String>();
                let ellipsis = if result_preview.len() > 300 { "…" } else { "" };
                bot.send_message(
                    tg_chat,
                    format!(
                        "✅ <b>{}</b> finished → <b>{}</b>\n<blockquote>{}{ellipsis}</blockquote>",
                        escape_html(&agent_id),
                        escape_html(&parent_agent_id),
                        escape_html(&preview),
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await
                .ok();
            }

            ServerEvent::PendingWrite { request_id, path, new_content, .. } => {
                let preview = truncate_chars(&new_content, 800);
                let text = format!(
                    "🔐 <b>Approval required</b>\n\
                     <b>Operation:</b> <code>{}</code>\n\n\
                     <b>Content:</b>\n<pre>{}</pre>",
                    escape_html(&path),
                    escape_html(&preview),
                );
                send_approval_keyboard(&bot, tg_chat, text, user_id.clone(), request_id, &shared).await;
            }

            ServerEvent::ApprovalRequired { request_id, tool_name, arguments, .. } => {
                let args_str = serde_json::to_string_pretty(&arguments)
                    .unwrap_or_else(|_| arguments.to_string());
                let args_preview = truncate_chars(&args_str, 600);
                let text = format!(
                    "🔐 <b>Approval required</b>\n\
                     <b>Tool:</b> <code>{}</code>\n\n\
                     <b>Arguments:</b>\n<pre>{}</pre>",
                    escape_html(&tool_name),
                    escape_html(&args_preview),
                );
                send_approval_keyboard(&bot, tg_chat, text, user_id.clone(), request_id, &shared).await;
            }

            ServerEvent::AgentQuestion { request_id, title, question, suggested_answers, .. } => {
                info!(request_id, %question, "telegram: forwarder received AgentQuestion");

                // Disable any previously-pending question for this chat.
                if let Some(prev) = shared.pending_questions.lock().await.remove(&chat_id) {
                    bot.edit_message_reply_markup(tg_chat, prev.message_id)
                        .reply_markup(InlineKeyboardMarkup::new(vec![vec![
                            InlineKeyboardButton::callback("⏭ Superseded by a newer question", "noop"),
                        ]]))
                        .await
                        .ok();
                }

                let header = format!("❓ <b>{}</b>\n{}", escape_html(&title), escape_html(&question));
                let keyboard = if suggested_answers.is_empty() {
                    None
                } else {
                    let buttons: Vec<Vec<InlineKeyboardButton>> = suggested_answers
                        .iter()
                        .enumerate()
                        .map(|(i, s)| vec![InlineKeyboardButton::callback(
                            s.clone(),
                            format!("ansidx:{request_id}:{i}"),
                        )])
                        .collect();
                    Some(InlineKeyboardMarkup::new(buttons))
                };
                let mut req = bot.send_message(tg_chat, header).parse_mode(ParseMode::Html);
                if let Some(kb) = keyboard {
                    req = req.reply_markup(kb);
                }
                match req.await {
                    Ok(m) => {
                        shared.pending_questions.lock().await.insert(chat_id, super::PendingQuestion {
                            user_id: user_id.clone(),
                            request_id,
                            message_id: m.id,
                            suggested_answers,
                        });
                    }
                    Err(e) => error!(error = %e, request_id, "telegram: failed to send AgentQuestion"),
                }
            }

            ServerEvent::LlmFailed { tried, last_error } => {
                let models = tried.join(", ");
                bot.send_message(
                    tg_chat,
                    format!(
                        "⚠️ <b>LLM unavailable</b>\nTried: <code>{}</code>\n{}",
                        escape_html(&models),
                        escape_html(&last_error),
                    ),
                )
                .parse_mode(ParseMode::Html)
                .await
                .ok();
            }

            // ToolDone, ToolError, FileChanged, Truncated, ModelFallback,
            // NewSession, ApprovalResolved (handled above) — silenced.
            _ => {}
        }
    }

    // Clean up: remove this user from the active forwarders set.
    shared.forwarders.lock().await.remove(&user_id);
    info!(user_id = %user_id, "telegram: forwarder exited");
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

// ── Callback query handler (button presses) ───────────────────────────────────

pub(crate) async fn callback_handler(
    bot:    Bot,
    q:      CallbackQuery,
    shared: Arc<TgShared>,
) -> ResponseResult<()> {
    let approval_msg = q
        .message
        .as_ref()
        .and_then(|m| m.regular_message())
        .map(|m| (m.chat.id, m.id));

    let Some((msg_chat_id, msg_id)) = approval_msg else {
        bot.answer_callback_query(q.id.clone()).await.ok();
        return Ok(());
    };

    let Some(data) = q.data.as_deref() else {
        bot.answer_callback_query(q.id.clone()).await.ok();
        return Ok(());
    };

    // ── Suggested-answer button (ask_user_clarification) ─────────────────────
    if let Some(rest) = data.strip_prefix("ansidx:") {
        let mut parts = rest.splitn(2, ':');
        let req_id  = parts.next().and_then(|s| s.parse::<i64>().ok());
        let idx_str = parts.next().and_then(|s| s.parse::<usize>().ok());
        if let (Some(request_id), Some(idx)) = (req_id, idx_str) {
            let pq_map = shared.pending_questions.lock().await;
            if let Some(pq) = pq_map.get(&msg_chat_id.0) {
                if pq.request_id == request_id {
                    let user_id    = pq.user_id.clone();
                    let answer     = pq.suggested_answers.get(idx).cloned().unwrap_or_default();
                    drop(pq_map);
                    shared.pending_questions.lock().await.remove(&msg_chat_id.0);

                    if let Some(handle) = shared.user_channel.resolve_user(&user_id).await {
                        handle.chat_hub().resolve_question("telegram", request_id, answer.clone()).await;
                        info!(request_id, %answer, "telegram: clarification answered via button");
                    } else {
                        warn!(user_id = %user_id, "telegram: user locked, cannot resolve clarification");
                    }
                    bot.edit_message_reply_markup(msg_chat_id, msg_id)
                        .reply_markup(InlineKeyboardMarkup::new(vec![vec![
                            InlineKeyboardButton::callback(format!("✅ {answer}"), "noop"),
                        ]]))
                        .await
                        .ok();
                }
            }
        }
        bot.answer_callback_query(q.id.clone()).await.ok();
        return Ok(());
    }

    // ── Approval buttons ──────────────────────────────────────────────────────
    enum ApprovalAction {
        Approve,
        Reject,
        BypassTime(u64),
        BypassSession,
    }

    let parsed: Option<(i64, ApprovalAction, &str)> =
        if let Some(id_str) = data.strip_prefix("approve:") {
            id_str.parse::<i64>().ok().map(|id| (id, ApprovalAction::Approve, "✅ Approved"))
        } else if let Some(id_str) = data.strip_prefix("reject:") {
            id_str.parse::<i64>().ok().map(|id| (id, ApprovalAction::Reject, "❌ Rejected"))
        } else if let Some(rest) = data.strip_prefix("bypass_time:") {
            let mut parts = rest.splitn(2, ':');
            let secs = parts.next().and_then(|s| s.parse::<u64>().ok());
            let id   = parts.next().and_then(|s| s.parse::<i64>().ok());
            secs.zip(id).map(|(s, id)| (id, ApprovalAction::BypassTime(s), "⏱ Bypass (timed)"))
        } else if let Some(id_str) = data.strip_prefix("bypass_session:") {
            id_str.parse::<i64>().ok().map(|id| (id, ApprovalAction::BypassSession, "🔄 Bypass (session)"))
        } else {
            None
        };

    if let Some((request_id, action, label)) = parsed {
        let stored = shared.pending_approvals.lock().await.remove(&msg_id);
        if let Some(pa) = stored {
            if pa.request_id == request_id {
                if let Some(handle) = shared.user_channel.resolve_user(&pa.user_id).await {
                    let approval = handle.approval();
                    match action {
                        ApprovalAction::Approve =>
                            approval.approve(request_id).await,
                        ApprovalAction::Reject =>
                            approval.reject(request_id, String::new()).await,
                        ApprovalAction::BypassTime(secs) =>
                            approval.approve_with_bypass(request_id, Some(secs)).await,
                        ApprovalAction::BypassSession =>
                            approval.approve_with_bypass(request_id, None).await,
                    }
                    info!(request_id, label, "telegram: approval resolved");
                    bot.delete_message(msg_chat_id, msg_id).await.ok();
                } else {
                    warn!(user_id = %pa.user_id, "telegram: user locked, cannot resolve approval");
                }
            }
        } else {
            warn!(request_id, "telegram: approval not found (already resolved?)");
        }
    }

    bot.answer_callback_query(q.id.clone()).await.ok();
    Ok(())
}
