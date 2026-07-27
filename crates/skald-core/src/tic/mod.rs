//! TIC — the background event processor, and the first of the **system agents**.
//!
//! A system agent runs on a user's behalf without being asked. TIC's job is to
//! look at the events the user's connectors pushed since the last tick (new
//! mail, a calendar change, a WhatsApp message), decide which of them are worth
//! interrupting the user for, and `notify()` those.
//!
//! **It is per-user, and that is not an implementation detail.** The events it
//! reads live in `mcp_events` inside the caller's own encrypted database, the
//! connectors that produced them run inside the caller's container, and the
//! notification it emits goes to the caller's own hub. This manager therefore
//! owns no timer and no user list: it exposes [`TicManager::run_for`], one tick
//! for one user, and the instance-wide scheduler
//! (`skald::wiring::spawn_system_agents`) decides who to run it for and when —
//! sequentially, skipping anyone whose database is still locked.
//!
//! The run is recorded in `system_agent_runs` in that same user's database, so
//! the trace of what TIC did for someone is readable by them and by nobody else.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tracing::{info, warn};

use core_api::interface_tool::{InterfaceTool, ToolFuture};
use core_api::{ConfigProperty, ConfigSet, PropertyType};

use crate::chat_hub::ChatHub;
use crate::config::TicConfig;
use crate::config_store::GlobalConfigManager;
use crate::db::{mcp_events, system_agent_runs};
use crate::run_context::{self, RunContext};
use crate::session::manager::ChatSessionManager;

/// The chat `source` TIC's ephemeral sessions carry. Kept distinct from the
/// user-facing sources (`web`, `talk`, `telegram`) so a tick never lands in a
/// conversation someone is reading.
const TIC_SOURCE: &str = "tic";
/// The agent id, in `agents/tic/`, and the `agent_id` of its `system_agent_runs` rows.
pub const TIC_AGENT: &str = "tic";

pub const TIC_ENABLED_KEY:          &str = "tic.enabled";
pub const TIC_SECURITY_GROUP_KEY:   &str = "tic.security_group";
pub const TIC_INTERVAL_MINUTES_KEY: &str = "tic.interval_minutes";

pub fn config_set() -> ConfigSet {
    ConfigSet {
        name:        "TIC Agent".into(),
        description: "TIC is a background agent that runs for every user, one at a time. For each \
                      user it reads the events their own connectors have pushed since the last run \
                      (new mail, calendar changes, incoming messages), decides — via an LLM call — \
                      which of them are worth surfacing, and sends those to that user as \
                      notifications. It reads only that user's events and writes only to their own \
                      conversation; a user who has not logged in since the last restart is skipped, \
                      because their database is still encrypted. Each run is recorded on the System \
                      agents page, visible to the user it ran for.".into(),
        properties:  vec![
            ConfigProperty {
                key:           TIC_ENABLED_KEY.into(),
                name:          "Enabled".into(),
                description:   "Enable or disable the TIC agent for the whole instance. When disabled, no events are processed for anyone.".into(),
                property_type: PropertyType::Bool,
                default_value: Some("true".into()),
            },
            ConfigProperty {
                key:           TIC_SECURITY_GROUP_KEY.into(),
                name:          "Security Group".into(),
                description:   "Tool permission group applied to each TIC run. It is re-checked against each user's own role: a user whose role does not allow this group runs under their role's default group instead. Leave empty to always use the role default.".into(),
                property_type: PropertyType::SecurityGroup,
                default_value: None,
            },
            ConfigProperty {
                key:           TIC_INTERVAL_MINUTES_KEY.into(),
                name:          "Check Interval (minutes)".into(),
                description:   "How often TIC starts a pass over all users, in minutes. Leave empty to use the value from config.yml (tic.interval_secs).".into(),
                property_type: PropertyType::Int,
                default_value: Some("15".into()),
            },
        ],
    }
}

/// What one tick did, for the run log. Counters only — never event contents.
pub struct TicRun {
    pub session_id:            i64,
    pub events_processed:      usize,
    pub notifications_emitted: usize,
}

impl TicRun {
    fn stats_json(&self) -> String {
        serde_json::json!({
            "events_processed":      self.events_processed,
            "notifications_emitted": self.notifications_emitted,
        })
        .to_string()
    }
}

pub struct TicManager {
    config:        TicConfig,
    config_store:  Arc<GlobalConfigManager>,
    /// `system.db` — read to resolve each user's role when validating the
    /// configured security group. Never written.
    registry_pool: Arc<SqlitePool>,
}

impl TicManager {
    pub fn new(
        config:        TicConfig,
        config_store:  Arc<GlobalConfigManager>,
        registry_pool: Arc<SqlitePool>,
    ) -> Arc<Self> {
        Arc::new(Self { config, config_store, registry_pool })
    }

    /// Instance-wide on/off switch. Read fresh each pass, so toggling it in
    /// Settings takes effect at the next pass with no restart.
    pub async fn is_enabled(&self) -> bool {
        match self.config_store.get(TIC_ENABLED_KEY).await {
            Ok(Some(v)) => v != "false",
            _           => true,
        }
    }

    /// Seconds between passes: the Settings value wins, else `config.yml`.
    pub async fn interval_secs(&self) -> u64 {
        if let Ok(Some(val)) = self.config_store.get(TIC_INTERVAL_MINUTES_KEY).await {
            if let Ok(mins) = val.parse::<u64>() {
                if mins > 0 {
                    return mins * 60;
                }
            }
        }
        self.config.interval_secs
    }

    /// One tick for one user, over that user's own runtime.
    ///
    /// `Ok(None)` means there was nothing to do — no pending events — and
    /// **nothing is written**: an idle tick must not leave a row behind, or the
    /// run log becomes a heartbeat instead of a history. Any other outcome opens
    /// a `system_agent_runs` row and closes it, failure included.
    pub async fn run_for(
        &self,
        user_id:  &str,
        pool:     &SqlitePool,
        sessions: &Arc<ChatSessionManager>,
        hub:      &Arc<ChatHub>,
    ) -> anyhow::Result<Option<TicRun>> {
        let events = mcp_events::pending_limited(pool, self.config.batch_size).await?;
        if events.is_empty() {
            return Ok(None);
        }

        let run_id = system_agent_runs::start(pool, TIC_AGENT).await?;
        let started = Instant::now();

        match self.tick(user_id, pool, sessions, hub, events).await {
            Ok(run) => {
                system_agent_runs::finish(
                    pool,
                    run_id,
                    system_agent_runs::STATUS_COMPLETED,
                    Some(run.session_id),
                    started.elapsed().as_millis() as i64,
                    Some(&run.stats_json()),
                    None,
                )
                .await?;
                info!(
                    user = %user_id,
                    events = run.events_processed,
                    notifications = run.notifications_emitted,
                    "TIC: tick complete",
                );
                Ok(Some(run))
            }
            Err(e) => {
                // Best-effort: the tick already failed, a failing log write must not
                // mask the original error.
                if let Err(log_err) = system_agent_runs::finish(
                    pool,
                    run_id,
                    system_agent_runs::STATUS_FAILED,
                    None,
                    started.elapsed().as_millis() as i64,
                    None,
                    Some(&e.to_string()),
                )
                .await
                {
                    warn!(user = %user_id, error = %log_err, "TIC: failed to record the failed run");
                }
                Err(e)
            }
        }
    }

    async fn tick(
        &self,
        user_id:  &str,
        pool:     &SqlitePool,
        sessions: &Arc<ChatSessionManager>,
        hub:      &Arc<ChatHub>,
        events:   Vec<mcp_events::McpEvent>,
    ) -> anyhow::Result<TicRun> {
        info!(user = %user_id, count = events.len(), "TIC: processing event batch");

        // Mark as processed BEFORE running the agent — a crash mid-turn then costs
        // this batch rather than replaying it forever. The loss is visible: the run
        // row closes as `failed` with the error.
        let ids: Vec<i64> = events.iter().map(|e| e.id).collect();
        mcp_events::mark_processed(pool, &ids).await?;

        let prompt = build_prompt(&events);
        let rc     = self.run_context_for(user_id).await;

        // A fresh ephemeral session per tick (agent_id = "tic", source = "tic").
        // ChatHub is bypassed: TIC is not a user-facing source and must not take
        // over the `sources` row of a conversation the user is having.
        let (session_id, _) = sessions
            .create_session(TIC_AGENT, TIC_SOURCE, false, true, rc.as_ref())
            .await?;
        let handler = sessions.get_or_create_handler(session_id).await?;
        handler.set_auto_deny_approvals();

        // The session's event stream has no subscriber, but the translator awaits
        // its sends — a receiver that is merely dropped, or kept and never polled,
        // wedges the turn at the channel's capacity. Drain it explicitly.
        let (tx, mut rx) = mpsc::channel(32);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let (notify, emitted) = counting_notify(Arc::clone(hub));

        handler
            .handle_message(
                &prompt,
                None,
                None,
                None,
                None,
                vec![notify],
                std::collections::HashMap::new(),
                tx,
                true,
                None,
                None,
            )
            .await?;

        Ok(TicRun {
            session_id,
            events_processed:      events.len(),
            notifications_emitted: emitted.load(Ordering::Relaxed),
        })
    }

    /// The security group for this user's tick.
    ///
    /// The configured group is an instance-wide admin setting, so it cannot be
    /// applied verbatim to somebody else's session: that would hand a restricted
    /// member's TIC run a tool set their role never granted. It goes through the
    /// same seam a persisted group does — [`run_context::reconcile_group_for_user`],
    /// which degrades it to the user's role default when their role does not allow
    /// it. With nothing configured we still start from the role default rather than
    /// `None`, because `None` means the catch-all group, which is *wider*.
    async fn run_context_for(&self, user_id: &str) -> Option<RunContext> {
        let configured = self
            .config_store
            .get(TIC_SECURITY_GROUP_KEY)
            .await
            .ok()
            .flatten()
            .filter(|g| !g.is_empty());

        match configured {
            Some(group) => {
                let wanted = RunContext::with_security_group(Some(group));
                run_context::reconcile_group_for_user(&self.registry_pool, user_id, Some(wanted)).await
            }
            None => run_context::role_default_run_context(&self.registry_pool, user_id).await,
        }
    }
}

/// Wrap the `notify` tool so the run log can report how many notifications the
/// tick actually produced, without the tool itself knowing it is being counted.
fn counting_notify(hub: Arc<ChatHub>) -> (InterfaceTool, Arc<AtomicUsize>) {
    let inner   = crate::tools::notify::make_tool(hub, "TIC");
    let counter = Arc::new(AtomicUsize::new(0));

    let handler = {
        let counter = Arc::clone(&counter);
        let call    = Arc::clone(&inner.handler);
        Arc::new(move |args: serde_json::Value| {
            let counter = Arc::clone(&counter);
            let fut     = call(args);
            Box::pin(async move {
                let out = fut.await;
                if out.is_ok() {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                out
            }) as ToolFuture
        })
    };

    (InterfaceTool { definition: inner.definition, handler }, counter)
}

// ── Prompt builder ─────────────────────────────────────────────────────────────

fn build_prompt(events: &[crate::db::mcp_events::McpEvent]) -> String {
    use std::fmt::Write;

    let n = events.len();
    let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    let mut out = format!("[TIC] {n} pending event(s) — {now}\n");

    for (i, ev) in events.iter().enumerate() {
        let _ = write!(
            out,
            "\n=== Event {}/{n} ===\nSource:   {}\nType:     {}\nReceived: {}\nPayload:\n{}\n",
            i + 1,
            ev.source,
            ev.method,
            ev.created_at,
            indent_payload(&ev.payload),
        );
    }

    out
}

/// Pretty-print a JSON payload with 2-space indent, falling back to raw string.
fn indent_payload(payload: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
        if let Ok(pretty) = serde_json::to_string_pretty(&v) {
            return pretty.lines().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n");
        }
    }
    format!("  {payload}")
}
