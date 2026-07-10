use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use core_api::events::{GlobalEvent, ServerEvent};

use super::TgShared;
use super::auth::load_wl;
use super::helpers::{escape_html, label_to_html, send_long};


/// Sends an inline keyboard for an approval request and records the request_id.
async fn send_approval_keyboard(
    bot:        &Bot,
    chat_id:    ChatId,
    text:       String,
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
        Ok(m)  => { shared.pending_approvals.lock().await.insert(m.id, request_id); }
        Err(e) => error!(error = %e, "telegram: failed to send approval message"),
    }
}

// ── Persistent background forwarder ──────────────────────────────────────────

/// Spawned once when the plugin starts.
/// Stays subscribed to the "telegram" broadcast channel forever, forwarding
/// events to the home chat_id.  This is the only subscriber — per-message
/// subscriptions are not used — so it also catches background notifications
/// that arrive without a user message triggering them.
///
/// Re-subscribes immediately after each `Done`/`Error` so no events from the
/// next turn are missed.  Safe because Tokio's cooperative scheduler guarantees
/// no other task runs between the re-subscription point and the next `await`,
/// and the processing mutex in `ChatSessionHandler` serialises turns.
pub(crate) async fn persistent_forwarder(
    bot:    Bot,
    shared: Arc<TgShared>,
    cancel: CancellationToken,
) {
    info!("telegram: persistent forwarder started");

    let mut rx = shared.chat_hub.events("telegram");

    // Single loop: rx is updated in-place on Done/Error so we never miss events
    // from the next turn (re-subscription happens before the async send).
    loop {
        let ge: GlobalEvent = tokio::select! {
            _ = cancel.cancelled() => {
                info!("telegram: persistent forwarder stopped");
                return;
            }
            result = rx.recv() => match result {
                Ok(e)                                       => e,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "telegram: persistent forwarder lagged");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        };

        // ApprovalResolved is handled regardless of source so Telegram removes its
        // keyboard even when the approval was resolved via web or REST.
        if let ServerEvent::ApprovalResolved { request_id, approved, .. } = ge.event {
            let label = if approved { "✅ Approved" } else { "❌ Rejected" };
            let mut pending = shared.pending_approvals.lock().await;
            if let Some((&msg_id, _)) = pending.iter().find(|(_, rid)| **rid == request_id) {
                let msg_id = msg_id;
                pending.remove(&msg_id);
                drop(pending);
                if let Some(cid) = resolve_chat_id(&shared).await {
                    bot.delete_message(cid, msg_id).await.ok();
                }
            }
            continue;
        }

        // All other events: only process if they belong to the "telegram" source.
        if ge.source.as_deref() != Some("telegram") {
            tracing::debug!(event_type = ge.event.type_name(), source = ?ge.source, "persistent_forwarder: skipping non-telegram event");
            continue;
        }

        let event = ge.event;
        tracing::debug!(event_type = event.type_name(), "persistent_forwarder: processing telegram event");

        // Resolve the destination chat_id (last known user, or first in whitelist).
        // For terminal events (Done/Error) with no known chat, still re-subscribe.
        let chat_id = match resolve_chat_id(&shared).await {
            Some(id) => id,
            None => {
                warn!(event_type = %event.type_name(), "telegram: persistent_forwarder — no chat_id resolved, dropping event");
                if matches!(event, ServerEvent::Done { .. } | ServerEvent::Error { .. }) {
                    rx = shared.chat_hub.events("telegram");
                }
                continue;
            }
        };

        match event {
            ServerEvent::Done { content, .. } => {
                // Re-subscribe BEFORE any await so we don't miss the next turn.
                rx = shared.chat_hub.events("telegram");
                if !content.trim().is_empty() {
                    send_long(&bot, chat_id, &content, Some(ParseMode::Html)).await;
                }
            }

            ServerEvent::Error { message } => {
                rx = shared.chat_hub.events("telegram");
                bot.send_message(
                    chat_id,
                    format!("⚠️ <b>Error:</b> {}", escape_html(&message)),
                )
                .parse_mode(ParseMode::Html)
                .await
                .ok();
            }

            ServerEvent::ToolStart { label_short, .. } => {
                bot.send_message(chat_id, format!("🔧 <i>{}</i>…", label_to_html(&label_short)))
                    .parse_mode(ParseMode::Html)
                    .await
                    .ok();
            }

            ServerEvent::Thinking { content, .. } => {
                if !content.trim().is_empty() {
                    send_long(&bot, chat_id, &content, Some(ParseMode::Html)).await;
                }
            }

            ServerEvent::AgentStart { agent_id, parent_agent_id, prompt_preview, .. } => {
                let preview  = prompt_preview.chars().take(300).collect::<String>();
                let ellipsis = if prompt_preview.len() > 300 { "…" } else { "" };
                bot.send_message(
                    chat_id,
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
                    chat_id,
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
                send_approval_keyboard(&bot, chat_id, text, request_id, &shared).await;
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
                send_approval_keyboard(&bot, chat_id, text, request_id, &shared).await;
            }

            ServerEvent::AgentQuestion { request_id, tool_call_id, title, question, suggested_answers, .. } => {
                info!(request_id, tool_call_id, %question, "telegram: persistent_forwarder received AgentQuestion");

                // If a previous question is still pending, disable its (now-dead)
                // buttons so tapping them doesn't silently no-op.
                if let Some(prev) = shared.pending_question.lock().await.take() {
                    bot.edit_message_reply_markup(chat_id, prev.message_id)
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
                let mut req = bot.send_message(chat_id, header).parse_mode(ParseMode::Html);
                if let Some(kb) = keyboard {
                    req = req.reply_markup(kb);
                }
                match req.await {
                    Ok(m) => {
                        info!(request_id, msg_id = m.id.0, "telegram: AgentQuestion sent to user, pending_question set");
                        *shared.pending_question.lock().await = Some(super::PendingQuestion {
                            request_id,
                            message_id: m.id,
                            suggested_answers,
                        });
                    }
                    Err(e) => error!(error = %e, request_id, "telegram: failed to send AgentQuestion to user"),
                }
            }

            ServerEvent::LlmFailed { tried, last_error } => {
                let models = tried.join(", ");
                bot.send_message(
                    chat_id,
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
}

/// Resolves the Telegram chat_id to use for outbound messages.
/// Prefers the last chat_id that sent a message; falls back to the first
/// whitelisted user.
async fn resolve_chat_id(shared: &TgShared) -> Option<ChatId> {
    if let Some(id) = *shared.home_chat_id.lock().await {
        return Some(id);
    }
    let wl = load_wl(&shared.secrets_dir).await;
    wl.whitelist.first().map(|&id| ChatId(id))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Truncates `s` to at most `max_chars` Unicode scalar values.
/// Appends `…` if truncated.  Never panics on multibyte UTF-8 content.
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
            let mut pq = shared.pending_question.lock().await;
            if let Some(pq_inner) = pq.as_ref() {
                if pq_inner.request_id == request_id {
                    let answer = pq_inner.suggested_answers.get(idx).cloned().unwrap_or_default();
                    drop(pq);
                    *shared.pending_question.lock().await = None;
                    shared.chat_hub.resolve_question("telegram", request_id, answer.clone()).await;
                    info!(request_id, %answer, "telegram: clarification answered via button");
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
        if let Some(stored_id) = stored {
            if stored_id == request_id {
                match action {
                    ApprovalAction::Approve =>
                        shared.approval.approve(request_id).await,
                    ApprovalAction::Reject =>
                        shared.approval.reject(request_id, String::new()).await,
                    ApprovalAction::BypassTime(secs) =>
                        shared.approval.approve_with_bypass(request_id, Some(secs)).await,
                    ApprovalAction::BypassSession =>
                        shared.approval.approve_with_bypass(request_id, None).await,
                }
                info!(request_id, label, "telegram: approval resolved");
                bot.delete_message(msg_chat_id, msg_id).await.ok();
            }
        } else {
            warn!(request_id, "telegram: approval not found (already resolved?)");
        }
    }

    bot.answer_callback_query(q.id.clone()).await.ok();
    Ok(())
}
