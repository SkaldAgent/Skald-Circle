/// Telegram plugin — connects the Skald LLM to a private Telegram bot.
///
/// # Pairing
/// Unknown users receive a pairing code in chat. The code is also written to
/// `secrets/telegram_whitelist.json` under `pending_pairings`. The main agent
/// (via `read_file` / `write_file`) can inspect that file and move the
/// `chat_id` into the `whitelist` array to complete the authorisation — no
/// code changes required, just a file edit.
///
/// # Human-in-the-loop approvals
/// Tool calls requiring approval emit a `PendingWrite` event; the plugin
/// forwards it to Telegram as an inline-keyboard message with
/// [✅ Approve] [❌ Reject] / [⏱ 15 min] [🔄 Session] buttons.
///
/// # Adding new message types
/// 1. Add a variant to `IncomingEvent` in `handlers.rs`.
/// 2. Handle it in `classify_message` (same file).
/// 3. Dispatch it in `message_handler` (same file).
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use teloxide::prelude::*;
use teloxide::types::MessageId;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use core_api::approval::ApprovalApi;
use core_api::chat_hub::ChatHubApi;
use core_api::command::CommandApi;
use core_api::location::LocationUpdater;
use core_api::plugin::{Plugin, PluginContext};
use core_api::transcribe::{Transcribe, TranscribeProvider};
use core_api::tts::TtsProvider;

mod attachments;
mod auth;
mod events;
mod handlers;
mod helpers;
mod tools;

/// Injected as extra system context for every Telegram turn.
/// Kept compact to minimise token overhead.
pub(crate) const TELEGRAM_FORMAT_CONTEXT: &str = "\
OUTPUT FORMAT — TELEGRAM HTML ONLY.\n\
Allowed tags: <b> <i> <u> <s> <code> <pre> <a> <blockquote>. \
Telegram supports NO other HTML and NO Markdown.\n\
FORBIDDEN (will appear as raw symbols): ** * _ ` # | and Markdown tables.\n\
• Headers → <b>text</b>\n\
• Structured data → bullet lists with •, never | tables\n\
• Escape & < > as &amp; &lt; &gt;";

/// Short reminder injected near the end of the message list to counter
/// instruction drift in long conversations.
pub(crate) const TELEGRAM_FORMAT_REMINDER: &str = "\
[FORMAT] Telegram HTML only: <b> <i> <code> <pre>. \
No Markdown: no ** * _ ` # |. No tables — use bullet lists.";

// ── Shared state injected into every teloxide handler ─────────────────────────

/// A pending `ask_user_clarification` question waiting for the user's reply.
pub(crate) struct PendingQuestion {
    pub(crate) request_id:        i64,
    pub(crate) message_id:        MessageId,
    /// Suggested answers (used to resolve the selection when the user taps a button).
    pub(crate) suggested_answers: Vec<String>,
}

pub(crate) struct TgShared {
    pub(crate) chat_hub:          Arc<dyn ChatHubApi>,
    /// Custom slash-command resolver (`commands/<name>/`). Read-only: lets the
    /// bot expand a recognised `/command` into a template before forwarding it to
    /// the LLM, mirroring the WS handler.
    pub(crate) command:           Arc<dyn CommandApi>,
    pub(crate) approval:          Arc<dyn ApprovalApi>,
    pub(crate) transcribe:        Arc<dyn TranscribeProvider>,
    pub(crate) tts:               Arc<dyn TtsProvider>,
    pub(crate) location:          Arc<dyn LocationUpdater>,
    /// MessageId of the approval message → request_id.
    pub(crate) pending_approvals: Mutex<HashMap<MessageId, i64>>,
    /// Currently active clarification question (at most one at a time per session).
    pub(crate) pending_question:  Mutex<Option<PendingQuestion>>,
    pub(crate) secrets_dir:       PathBuf,
    /// Base directory for file attachments: `<data_root>/uploads/telegram/`.
    pub(crate) uploads_dir:       PathBuf,
    /// Last chat_id that sent a message — used as the target for background notifications.
    /// Set on every incoming message; read by the persistent event forwarder.
    pub(crate) home_chat_id:      Mutex<Option<ChatId>>,
}

impl TgShared {
    pub(crate) async fn transcriber(&self) -> Option<Arc<dyn Transcribe>> {
        self.transcribe.get().await
    }
}

// ── Plugin struct ─────────────────────────────────────────────────────────────

pub struct TelegramPlugin {
    secrets_dir: PathBuf,
    /// Bot token — set by reload() before start() is called.
    token:       Mutex<String>,
    running:     Arc<AtomicBool>,
    cancel:      Mutex<Option<CancellationToken>>,
    handle:      Mutex<Option<JoinHandle<()>>>,
}

impl TelegramPlugin {
    pub fn new(secrets_dir: impl Into<PathBuf>) -> Self {
        Self {
            secrets_dir: secrets_dir.into(),
            token:       Mutex::new(String::new()),
            running:     Arc::new(AtomicBool::new(false)),
            cancel:      Mutex::new(None),
            handle:      Mutex::new(None),
        }
    }
}

#[async_trait]
impl Plugin for TelegramPlugin {
    fn id(&self)          -> &str { "telegram" }
    fn name(&self)        -> &str { "Telegram Bot" }
    fn description(&self) -> &str {
        "Private Telegram bot. Forwards messages to the LLM; supports HITL approval via inline keyboards."
    }
    fn is_running(&self) -> bool { self.running.load(Ordering::Relaxed) }

    fn config_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "token": {
                    "type":        "string",
                    "title":       "Bot Token",
                    "description": "Telegram bot token from @BotFather",
                    "sensitive":   true
                }
            },
            "required": ["token"]
        })
    }

    fn as_any(&self) -> &dyn std::any::Any { self }

    fn as_arc_any(self: Arc<Self>) -> Arc<dyn std::any::Any + Send + Sync> { self }

    async fn reload(&self, enabled: bool, config: Value, ctx: PluginContext) -> Result<()> {
        let new_token = config["token"].as_str().unwrap_or("").to_string();
        let old_token = self.token.lock().await.clone();
        let is_running = self.is_running();

        match (enabled, is_running) {
            (true, false) => {
                anyhow::ensure!(!new_token.is_empty(),
                    "telegram: cannot start — `token` is missing from config");
                *self.token.lock().await = new_token;
                self.start(ctx).await?;
            }
            (false, true) => {
                self.stop().await?;
            }
            (true, true) => {
                if new_token != old_token {
                    info!("telegram: token changed — restarting");
                    self.stop().await?;
                    *self.token.lock().await = new_token;
                    self.start(ctx).await?;
                }
            }
            (false, false) => {}
        }
        Ok(())
    }

    async fn start(&self, ctx: PluginContext) -> Result<()> {
        if self.running.load(Ordering::Relaxed) {
            return Ok(());
        }
        let token = self.token.lock().await.clone();
        if token.is_empty() {
            anyhow::bail!("telegram: token is empty — set it via the plugins API");
        }

        let uploads_dir = self.secrets_dir
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .join("uploads")
            .join("telegram");

        // Register "telegram" source with ChatHub (idempotent).
        // ChatHub restores the active session from the sources table automatically.
        ctx.chat_hub.register("telegram").await;
        info!("telegram: registered with ChatHub");

        let shared = Arc::new(TgShared {
            chat_hub:          Arc::clone(&ctx.chat_hub),
            command:           Arc::clone(&ctx.command),
            approval:          Arc::clone(&ctx.approval),
            transcribe:        Arc::clone(&ctx.transcribe),
            tts:               Arc::clone(&ctx.tts_provider),
            location:          Arc::clone(&ctx.location),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_question:  Mutex::new(None),
            secrets_dir:       self.secrets_dir.clone(),
            uploads_dir,
            home_chat_id:      Mutex::new(None),
        });

        let bot    = Bot::new(&token);
        let cancel = CancellationToken::new();

        tokio::spawn(events::persistent_forwarder(
            bot.clone(),
            Arc::clone(&shared),
            cancel.clone(),
        ));

        let hub_clone = Arc::clone(&ctx.chat_hub);
        tokio::spawn(async move {
            if let Err(e) = hub_clone.resume("telegram").await {
                tracing::warn!(error = %e, "telegram: startup resume failed");
            }
        });

        let cancel_clone  = cancel.clone();
        let cancel_wdg    = cancel.clone();
        let running_clone = Arc::clone(&self.running);
        self.running.store(true, Ordering::Relaxed);

        let handler = dptree::entry()
            .branch(Update::filter_message().endpoint(handlers::message_handler))
            .branch(Update::filter_edited_message().endpoint(handlers::edited_message_handler))
            .branch(Update::filter_callback_query().endpoint(events::callback_handler));

        let secrets_dir_wdg = self.secrets_dir.clone();
        let bot_wdg         = bot.clone();

        let task = tokio::spawn(async move {
            let mut dispatcher = Dispatcher::builder(bot, handler)
                .dependencies(dptree::deps![shared])
                .build();

            info!("telegram plugin: dispatcher starting");
            tokio::select! {
                _ = cancel_clone.cancelled()                                        => info!("telegram plugin: cancellation received"),
                _ = dispatcher.dispatch()                                           => warn!("telegram plugin: dispatcher exited unexpectedly"),
                _ = auth::whitelist_watchdog(bot_wdg, secrets_dir_wdg, cancel_wdg) => {}
            }
            running_clone.store(false, Ordering::Relaxed);
            info!("telegram plugin: stopped");
        });

        *self.cancel.lock().await = Some(cancel);
        *self.handle.lock().await = Some(task);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if let Some(token) = self.cancel.lock().await.take() {
            token.cancel();
        }
        if let Some(h) = self.handle.lock().await.take() {
            let _ = h.await;
        }
        self.running.store(false, Ordering::Relaxed);
        Ok(())
    }
}
