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
//! owns no timer and no user list: it implements
//! [`SystemAgent`](crate::system_agents::SystemAgent), one pass for one user,
//! and the instance-wide scheduler (`skald::wiring::spawn_system_agents`)
//! decides who to run it for and when — sequentially, skipping anyone whose
//! database is still locked.
//!
//! The run is recorded in `system_agent_runs` in that same user's database, so
//! the trace of what TIC did for someone is readable by them and by nobody else.
//! Opening and closing that row is [`crate::system_agents::run_and_record`]'s
//! job, not TIC's: every agent needs it identically, and the ordering rules
//! around it are subtle enough that one copy is the only safe number.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::SqlitePool;
use tracing::info;

use core_api::{ConfigProperty, ConfigSet, PropertyType};

use crate::config::TicConfig;
use crate::config_store::GlobalConfigManager;
use crate::db::mcp_events;
use crate::system_agents::{
    AgentOutcome, AgentRunCtx, AgentScope, SystemAgent, configured_run_context,
    enabled_from_config, enabled_property, interval_from_config, run_ephemeral_turn,
    security_group_property,
};

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
            enabled_property(
                TIC_ENABLED_KEY,
                "Enable or disable the TIC agent for the whole instance. When disabled, no events \
                 are processed for anyone.",
            ),
            security_group_property(TIC_SECURITY_GROUP_KEY),
            ConfigProperty {
                key:           TIC_INTERVAL_MINUTES_KEY.into(),
                name:          "Check interval (minutes)".into(),
                description:   "How long between passes for each user, in minutes. Counted per \
                                person from their own last pass. Leave empty to use the value from \
                                config.yml (tic.interval_secs)."
                    .into(),
                property_type: PropertyType::Int,
                default_value: Some("15".into()),
            },
        ],
        owner:       Some(TIC_AGENT.into()),
    }
}

/// What one tick did, for the run log. Counters only — never event contents.
pub struct TicRun {
    pub session_id:            i64,
    pub events_processed:      usize,
    pub notifications_emitted: usize,
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

    /// One tick for one user, over that user's own runtime.
    async fn tick(&self, ctx: &AgentRunCtx<'_>) -> Result<TicRun> {
        let events = mcp_events::pending_limited(ctx.pool, self.config.batch_size).await?;
        info!(user = %ctx.user_id, count = events.len(), "TIC: processing event batch");

        // Mark as processed BEFORE running the agent — a crash mid-turn then costs
        // this batch rather than replaying it forever. The loss is visible: the run
        // row closes as `failed` with the error.
        let ids: Vec<i64> = events.iter().map(|e| e.id).collect();
        mcp_events::mark_processed(ctx.pool, &ids).await?;

        let rc = configured_run_context(
            &self.config_store,
            &self.registry_pool,
            TIC_SECURITY_GROUP_KEY,
            ctx.user_id,
        )
        .await;

        let (session_id, notified) = run_ephemeral_turn(
            TIC_AGENT,
            TIC_SOURCE,
            &build_prompt(&events),
            rc.as_ref(),
            "TIC",
            ctx,
        )
        .await?;

        Ok(TicRun {
            session_id,
            events_processed:      events.len(),
            notifications_emitted: notified,
        })
    }
}

#[async_trait]
impl SystemAgent for TicManager {
    fn id(&self) -> &'static str { TIC_AGENT }

    fn scope(&self) -> AgentScope { AgentScope::PerUser }

    fn config_set(&self) -> ConfigSet { config_set() }

    fn interval_key(&self) -> &'static str { TIC_INTERVAL_MINUTES_KEY }

    async fn is_enabled(&self) -> bool {
        enabled_from_config(&self.config_store, TIC_ENABLED_KEY).await
    }

    /// Seconds between passes: the Settings value (minutes) wins, else `config.yml`.
    async fn interval_secs(&self) -> u64 {
        interval_from_config(
            &self.config_store,
            TIC_INTERVAL_MINUTES_KEY,
            60,
            self.config.interval_secs,
        )
        .await
    }

    /// No pending events means no tick at all — and no row. The batch is re-read
    /// in [`TicManager::tick`]; it is one indexed query on a small table, and
    /// paying it twice is cheaper than a trait shaped around carrying the rows.
    async fn has_work(&self, ctx: &AgentRunCtx<'_>) -> Result<bool> {
        let events = mcp_events::pending_limited(ctx.pool, self.config.batch_size).await?;
        Ok(!events.is_empty())
    }

    async fn run(&self, ctx: &AgentRunCtx<'_>) -> Result<AgentOutcome> {
        let run = self.tick(ctx).await?;
        Ok(AgentOutcome {
            session_id: Some(run.session_id),
            stats:      serde_json::json!({
                "events_processed":      run.events_processed,
                "notifications_emitted": run.notifications_emitted,
            }),
        })
    }
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
