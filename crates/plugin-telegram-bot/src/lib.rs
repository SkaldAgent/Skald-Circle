/// Telegram plugin — connects the Skald LLM to a private Telegram bot.
///
/// # Multi-user architecture (blueprint §13)
///
/// One bot serves many Telegram chats, each bound to a Skald user via the
/// `chat_id ↔ user_id` pairing stored in the config table (key `"telegram"`).
/// Incoming messages resolve the user's per-user context via
/// [`UserChannelApi`], then dispatch through that user's `ChatHub`. A per-user
/// forwarder subscribes to the user's event stream and routes `ServerEvent`s
/// back to the bound Telegram chat.
///
/// # Pairing
///
/// Unknown chats receive a pairing code. The user links their own account by
/// pasting the code in the Plugins page of the web app (the plugin's
/// `user_config_schema` / `update_user_config` hook); the admin's agent can
/// also bind a chat via the `telegram_pairing` tool (category `Config`). The
/// binding is written to the config table; the resulting `ConfigKeyUpdated`
/// event reloads the in-memory cache instantly.
///
/// # Human-in-the-loop approvals
///
/// Tool calls requiring approval emit a `PendingWrite` / `ApprovalRequired`
/// event; the per-user forwarder sends it to Telegram as an inline-keyboard
/// message. Button presses resolve the approval through that user's
/// `ApprovalApi`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use teloxide::prelude::*;
use teloxide::types::MessageId;
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use core_api::command::CommandApi;
use core_api::config_api::ConfigApi;
use core_api::location::LocationUpdater;
use core_api::plugin::{Plugin, PluginContext};
use core_api::transcribe::TranscribeProvider;
use core_api::tts::TtsProvider;
use core_api::user_channel::UserChannelApi;

mod attachments;
mod auth;
mod events;
mod handlers;
mod helpers;
mod tools;

/// The plugin id — the key into `plugin_access` / `plugin_user_configs` and the
/// value returned by [`Plugin::id`]. Kept in one place so the runtime access
/// check and the registration id can never drift apart.
pub(crate) const PLUGIN_ID: &str = "telegram";

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
    pub(crate) user_id:          String,
    pub(crate) request_id:       i64,
    pub(crate) message_id:       MessageId,
    /// Suggested answers (used to resolve the selection when the user taps a button).
    pub(crate) suggested_answers: Vec<String>,
}

/// A pending tool-call approval shown as an inline keyboard.
pub(crate) struct PendingApproval {
    pub(crate) user_id:    String,
    pub(crate) request_id: i64,
}

/// Global state shared across all Telegram handlers and the per-user forwarders.
///
/// Per-user state (ChatHub, ApprovalApi, event stream) is resolved at runtime
/// via [`UserChannelApi`] — it is NOT held here. Only global capabilities and
/// pairing/multiplexing state live in `TgShared`.
pub(crate) struct TgShared {
    // ── Global capabilities ──
    pub(crate) user_channel: Arc<dyn UserChannelApi>,
    pub(crate) command:      Arc<dyn CommandApi>,
    pub(crate) config:       Arc<dyn ConfigApi>,
    pub(crate) transcribe:   Arc<dyn TranscribeProvider>,
    pub(crate) tts:          Arc<dyn TtsProvider>,
    pub(crate) location:     Arc<dyn LocationUpdater>,
    pub(crate) uploads_dir:  PathBuf,

    // ── Pairing / bindings (config-table-backed, cached in memory) ──
    pub(crate) bindings:     RwLock<auth::TelegramConfig>,

    // ── Per-chat pending state ──
    /// Approval message_id → pending approval (carries user_id for routing).
    pub(crate) pending_approvals: Mutex<HashMap<MessageId, PendingApproval>>,
    /// chat_id → pending clarification question (at most one per chat).
    pub(crate) pending_questions: Mutex<HashMap<i64, PendingQuestion>>,

    // ── Forwarder tracking ──
    /// user_ids with an active per-user forwarder task.
    pub(crate) forwarders: Mutex<HashSet<String>>,
}

impl TgShared {
    pub(crate) async fn transcriber(&self) -> Option<Arc<dyn core_api::transcribe::Transcribe>> {
        self.transcribe.get().await
    }

    /// Looks up the `user_id` bound to a Telegram `chat_id`, if any.
    pub(crate) async fn user_for_chat(&self, chat_id: i64) -> Option<String> {
        self.bindings.read().await
            .bindings.iter()
            .find(|b| b.chat_id == chat_id)
            .map(|b| b.user_id.clone())
    }

    /// Whether a bound `user_id` may still use this plugin. A binding only says
    /// "this chat belongs to this user"; access is a separate, admin-revocable
    /// grant (`plugin_access`). Enforced on every inbound message so a revoke
    /// takes effect immediately — the binding is left intact so a re-grant
    /// restores service without forcing the user to pair again.
    pub(crate) async fn user_authorized(&self, user_id: &str) -> bool {
        self.user_channel.plugin_access(PLUGIN_ID, user_id).await
    }
}

// ── Plugin struct ─────────────────────────────────────────────────────────────

pub struct TelegramPlugin {
    /// Bot token — set by reload() before start() is called.
    token:       Mutex<String>,
    running:     Arc<AtomicBool>,
    cancel:      Mutex<Option<CancellationToken>>,
    handle:      Mutex<Option<JoinHandle<()>>>,
    /// Runtime shared state, populated by `start()`. Accessible to the pairing
    /// tool so it can write bindings before/after the dispatcher is running.
    shared:      std::sync::OnceLock<Arc<TgShared>>,
}

impl TelegramPlugin {
    pub fn new() -> Self {
        Self {
            token:   Mutex::new(String::new()),
            running: Arc::new(AtomicBool::new(false)),
            cancel:  Mutex::new(None),
            handle:  Mutex::new(None),
            shared:  std::sync::OnceLock::new(),
        }
    }

    /// Returns the shared runtime state if the plugin is running.
    pub(crate) fn shared(&self) -> Option<&Arc<TgShared>> {
        self.shared.get()
    }
}

#[async_trait]
impl Plugin for TelegramPlugin {
    fn id(&self)          -> &str { PLUGIN_ID }
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

    fn user_config_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pairing_code": {
                    "type":        "string",
                    "title":       "Pairing code",
                    "description": "Send any message to the bot — it replies with a 6-character code. Paste it here to link your Telegram chat."
                }
            },
            "required": ["pairing_code"]
        })
    }

    /// Self-service pairing: the user pastes the code the bot replied with,
    /// we turn it into a `chat_id → user_id` binding (same write path as the
    /// `telegram_pairing` tool) and store a status blob for the UI.
    async fn update_user_config(&self, user_id: &str, config: Value, ctx: &PluginContext) -> Result<()> {
        let code = config.get("pairing_code").and_then(Value::as_str).unwrap_or("").trim();
        anyhow::ensure!(!code.is_empty(), "telegram: `pairing_code` is required");
        let shared = self.shared()
            .ok_or_else(|| anyhow::anyhow!("telegram: the bot is not running — ask the admin to check the plugin"))?
            .clone();
        let mut cfg = auth::load_config(&*shared.config).await.unwrap_or_default();
        let chat_id = auth::apply_pairing_code(&mut cfg, code, user_id)?;
        auth::save_config(&*shared.config, &cfg).await?;
        ctx.user_config
            .set(self.id(), user_id, json!({ "linked": true, "chat_id": chat_id }))
            .await?;
        info!(user_id, chat_id, "telegram: user self-paired via the web UI");
        Ok(())
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

        let uploads_dir = std::env::current_dir()
            .unwrap_or_default()
            .join("uploads")
            .join("telegram");

        // Load bindings from the config table (or default if absent).
        let telegram_config = auth::load_config(&*ctx.config).await
            .unwrap_or_default();
        info!(
            bindings = telegram_config.bindings.len(),
            pending   = telegram_config.pending_pairings.len(),
            "telegram: config loaded",
        );

        let shared = Arc::new(TgShared {
            user_channel:      Arc::clone(&ctx.user_channel),
            command:           Arc::clone(&ctx.command),
            config:            Arc::clone(&ctx.config),
            transcribe:        Arc::clone(&ctx.transcribe),
            tts:               Arc::clone(&ctx.tts_provider),
            location:          Arc::clone(&ctx.location),
            uploads_dir,
            bindings:          RwLock::new(telegram_config),
            pending_approvals: Mutex::new(HashMap::new()),
            pending_questions: Mutex::new(HashMap::new()),
            forwarders:        Mutex::new(HashSet::new()),
        });

        let _ = self.shared.set(Arc::clone(&shared));

        let bot    = Bot::new(&token);
        let cancel = CancellationToken::new();

        // Config listener: reloads bindings when the "telegram" config key
        // changes (e.g. the pairing tool writes a new binding).
        {
            let shared_c  = Arc::clone(&shared);
            let cancel_c  = cancel.clone();
            let bus_rx    = ctx.system_bus.subscribe();
            tokio::spawn(auth::config_listener(shared_c, bus_rx, cancel_c));
        }

        // Spawn forwarders for already-unlocked paired users.
        {
            let shared_c = Arc::clone(&shared);
            let bot_c    = bot.clone();
            let cancel_c = cancel.clone();
            tokio::spawn(async move {
                events::spawn_forwarders_for_bound_users(&bot_c, &shared_c, &cancel_c).await;
            });
        }

        let cancel_clone  = cancel.clone();
        let running_clone = Arc::clone(&self.running);
        self.running.store(true, Ordering::Relaxed);

        let handler = dptree::entry()
            .branch(Update::filter_message().endpoint(handlers::message_handler))
            .branch(Update::filter_edited_message().endpoint(handlers::edited_message_handler))
            .branch(Update::filter_callback_query().endpoint(events::callback_handler));

        let task = tokio::spawn(async move {
            let mut dispatcher = Dispatcher::builder(bot, handler)
                .dependencies(dptree::deps![shared])
                .build();

            info!("telegram plugin: dispatcher starting");
            tokio::select! {
                _ = cancel_clone.cancelled() => info!("telegram plugin: cancellation received"),
                _ = dispatcher.dispatch()    => warn!("telegram plugin: dispatcher exited unexpectedly"),
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

    fn tools(self: Arc<Self>) -> Vec<Arc<dyn core_api::tool::Tool>> {
        vec![Arc::new(tools::TelegramPairingTool::new(self))]
    }
}
