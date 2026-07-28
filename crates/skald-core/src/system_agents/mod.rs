//! System agents — the background agents the instance runs on a user's behalf.
//!
//! A system agent runs without being asked. TIC was the first, and everything it
//! needed turned out to be general: an on/off switch, an interval, a security
//! group reconciled against the user's own role, an ephemeral session, and a run
//! recorded in the user's own database. This module is that shape, extracted, so
//! a second agent is a [`SystemAgent`] impl and nothing else — no timer of its
//! own, no bookkeeping of its own, no scheduler of its own.
//!
//! **The unit of work is one agent for one user.** The instance-wide scheduler
//! (`skald::wiring::spawn_system_agents`) decides who and when; an agent decides
//! only what. That split is what made TIC per-user correct, and it is why an
//! agent never sees the user list.
//!
//! ## Why the work is split in three
//!
//! [`run_and_record`] wraps every pass, and the order of its steps is
//! load-bearing:
//!
//! 1. **Mark the attempt** ([`db::system_agent_state`]) — always, before
//!    anything else, so due-ness advances even for a pass that turns out to have
//!    nothing to do. An agent that only recorded productive runs would be asked
//!    again on every tick.
//! 2. **Ask [`SystemAgent::has_work`]** — a cheap look before any row is opened.
//!    `false` writes nothing at all: an idle tick must not leave a trace, or the
//!    run log stops being a history and becomes a heartbeat.
//! 3. **Open the run row, then work.** The `start`/`finish` split means a crash
//!    mid-pass leaves a visible `running` row, swept to `failed` by the next
//!    `start` for that agent — safe only because the scheduler is sequential and
//!    single-instance.

pub mod memory_lint;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::SqlitePool;
use tokio::sync::mpsc;
use tracing::{info, warn};

use core_api::interface_tool::{InterfaceTool, ToolFuture};
use core_api::{ConfigProperty, ConfigSet, PropertyType};

use crate::chat_hub::ChatHub;
use crate::config_store::GlobalConfigManager;
use crate::db::{system_agent_runs, system_agent_state};
use crate::run_context::{self, RunContext};
use crate::session::manager::ChatSessionManager;

/// Who a pass runs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentScope {
    /// One pass per user, over that user's own runtime. The default shape: the
    /// data is theirs, the notification is theirs, the trace is theirs.
    PerUser,
    /// One pass for the whole instance, run inside the admin's runtime.
    ///
    /// For work over something that has **no owner** — the shared memory store
    /// being the case that forced this variant. Such a pass still has to run
    /// *somewhere*: an ownerless run would write its trace into `system.db`,
    /// which `GET /api/system-agents/runs` shows to nobody (scoped on the
    /// caller's own pool, by design), and its `notify()` would have no
    /// recipient. Attributing it to the admin keeps the whole per-user surface
    /// working unchanged, at the price of needing an admin who has logged in
    /// since the last restart.
    Instance,
}

/// What one pass did, for the run log.
pub struct AgentOutcome {
    /// The ephemeral session the pass ran in, so the UI can link to it.
    pub session_id: Option<i64>,
    /// The agent's own counters. Never the contents of what it read.
    pub stats: serde_json::Value,
}

/// One user's runtime, unpacked from their `UserContext` by the scheduler.
pub struct AgentRunCtx<'a> {
    pub user_id:  &'a str,
    /// The user's own (unlocked) database.
    pub pool:     &'a SqlitePool,
    pub sessions: &'a Arc<ChatSessionManager>,
    pub hub:      &'a Arc<ChatHub>,
}

#[async_trait]
pub trait SystemAgent: Send + Sync {
    /// Directory name under `agents/`, and the `agent_id` of its rows.
    fn id(&self) -> &'static str;

    fn scope(&self) -> AgentScope;

    /// The settings shown on this agent's tab of the System agents page. Must be
    /// `owned_by(self.id())`, or it lands on the general Config page instead.
    fn config_set(&self) -> ConfigSet;

    /// The config key holding the interval. The scheduler watches it so a change
    /// in the UI reschedules without a restart.
    fn interval_key(&self) -> &'static str;

    /// Instance-wide on/off switch, re-read every pass.
    async fn is_enabled(&self) -> bool;

    /// How long between passes **for one user**, in seconds.
    async fn interval_secs(&self) -> u64;

    /// Cheap look at whether this pass would do anything, before a run row is
    /// opened. `false` means "nothing to do" and leaves no trace behind.
    async fn has_work(&self, ctx: &AgentRunCtx<'_>) -> Result<bool>;

    /// The pass itself. The run row is already open; returning `Err` closes it
    /// as `failed` with the message.
    async fn run(&self, ctx: &AgentRunCtx<'_>) -> Result<AgentOutcome>;
}

/// Every system agent the instance runs, in pass order.
///
/// The **one** place the set is enumerated. The scheduler takes this list, and
/// [`config_sets`] derives the settings surface from it, so an agent cannot exist
/// in one and be missing from the other — the failure that would otherwise look
/// like "the agent runs but has no settings" or "the settings page edits keys
/// nothing reads".
pub fn registry(
    tic_config:    crate::config::TicConfig,
    config_store:  Arc<GlobalConfigManager>,
    registry_pool: Arc<SqlitePool>,
) -> Vec<Arc<dyn SystemAgent>> {
    vec![
        crate::tic::TicManager::new(
            tic_config,
            Arc::clone(&config_store),
            Arc::clone(&registry_pool),
        ),
        memory_lint::MemoryLintAgent::private(
            Arc::clone(&config_store),
            Arc::clone(&registry_pool),
        ),
        memory_lint::MemoryLintAgent::shared(config_store, registry_pool),
    ]
}

/// The config sets of every system agent, in the same order as [`registry`].
///
/// A free function rather than `registry(..).map(|a| a.config_set())` because
/// `Runtime::bootstrap` needs the settings surface before it has the runtime
/// dependencies an agent is built from. `registry_and_config_sets_agree` is what
/// keeps the two honest.
pub fn config_sets() -> Vec<ConfigSet> {
    vec![
        crate::tic::config_set(),
        memory_lint::private_config_set(),
        memory_lint::shared_config_set(),
    ]
}

/// Is `agent` due for this user? `true` when it has never run here, or when the
/// last attempt is older than the configured interval.
///
/// Read from the database rather than an in-memory deadline, which is what makes
/// a weekly agent survive a restart — see the `system_agent_state` table comment.
pub async fn is_due(agent: &dyn SystemAgent, pool: &SqlitePool) -> bool {
    let interval = agent.interval_secs().await as i64;
    match system_agent_state::seconds_since_attempt(pool, agent.id()).await {
        Ok(Some(elapsed)) => elapsed >= interval,
        // Never attempted here — due now.
        Ok(None) => true,
        // Unreadable state: run it. A spurious pass is recoverable; an agent that
        // silently stops running is not.
        Err(e) => {
            warn!(agent = agent.id(), error = %e, "system-agents: cannot read schedule state, running anyway");
            true
        }
    }
}

/// Run one pass and record it. See the module docs for why the steps are ordered
/// the way they are.
///
/// `Ok(None)` means the pass had nothing to do and wrote no run row.
pub async fn run_and_record(
    agent: &dyn SystemAgent,
    ctx:   &AgentRunCtx<'_>,
) -> Result<Option<AgentOutcome>> {
    // Step 1 — the attempt counts even if there is nothing to do, or an idle
    // agent is asked again on every single tick.
    if let Err(e) = system_agent_state::mark_attempt(ctx.pool, agent.id()).await {
        warn!(agent = agent.id(), user = %ctx.user_id, error = %e,
              "system-agents: could not record the attempt");
    }

    // Step 2 — nothing to do leaves no trace.
    if !agent.has_work(ctx).await? {
        return Ok(None);
    }

    // Step 3 — open the row, then work.
    let run_id  = system_agent_runs::start(ctx.pool, agent.id()).await?;
    let started = Instant::now();

    match agent.run(ctx).await {
        Ok(outcome) => {
            system_agent_runs::finish(
                ctx.pool,
                run_id,
                system_agent_runs::STATUS_COMPLETED,
                outcome.session_id,
                started.elapsed().as_millis() as i64,
                Some(&outcome.stats.to_string()),
                None,
            )
            .await?;
            info!(agent = agent.id(), user = %ctx.user_id, stats = %outcome.stats,
                  "system-agents: pass complete");
            Ok(Some(outcome))
        }
        Err(e) => {
            // Best-effort: the pass already failed, and a failing log write must
            // not mask the original error.
            if let Err(log_err) = system_agent_runs::finish(
                ctx.pool,
                run_id,
                system_agent_runs::STATUS_FAILED,
                None,
                started.elapsed().as_millis() as i64,
                None,
                Some(&e.to_string()),
            )
            .await
            {
                warn!(agent = agent.id(), user = %ctx.user_id, error = %log_err,
                      "system-agents: failed to record the failed pass");
            }
            Err(e)
        }
    }
}

// ── Shared machinery ───────────────────────────────────────────────────────────

/// Run one ephemeral turn of `agent_id` and return `(session_id, notifications
/// emitted)`.
///
/// Every system agent talks to its user the same way: a throwaway session that
/// `ChatHub` never sees, approvals auto-denied because nobody is watching, and
/// `notify()` as the only way out. Sharing it is what keeps a new agent from
/// re-deriving the two subtleties below.
pub async fn run_ephemeral_turn(
    agent_id:     &str,
    source:       &str,
    prompt:       &str,
    run_context:  Option<&RunContext>,
    notify_label: &str,
    ctx:          &AgentRunCtx<'_>,
) -> Result<(i64, usize)> {
    // A fresh ephemeral session per pass. ChatHub is bypassed on purpose: a
    // system agent is not a user-facing source and must not take over the
    // `sources` row of a conversation the user is having.
    let (session_id, _) = ctx
        .sessions
        .create_session(agent_id, source, false, true, run_context)
        .await?;
    let handler = ctx.sessions.get_or_create_handler(session_id).await?;

    // Nobody is at the keyboard to answer an approval card, so anything the
    // rules gate is denied rather than left hanging forever.
    handler.set_auto_deny_approvals();

    // The session's event stream has no subscriber, but the translator awaits its
    // sends — a receiver merely dropped, or kept and never polled, wedges the
    // turn at the channel's capacity. Drain it explicitly.
    let (tx, mut rx) = mpsc::channel(32);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });

    let (notify, emitted) = counting_notify(Arc::clone(ctx.hub), notify_label);

    handler
        .handle_message(
            prompt,
            None,
            None,
            None,
            None,
            vec![notify],
            HashMap::new(),
            tx,
            true,
            None,
            None,
        )
        .await?;

    Ok((session_id, emitted.load(Ordering::Relaxed)))
}

/// The security group for one user's pass.
///
/// The configured group is an instance-wide admin setting, so it cannot be
/// applied verbatim to somebody else's session: that would hand a restricted
/// member's background agent a tool set their role never granted. It goes
/// through the same seam a persisted group does —
/// [`run_context::reconcile_group_for_user`] — which degrades it to the user's
/// role default when their role does not allow it. With nothing configured we
/// still start from the role default rather than `None`, because `None` means
/// the catch-all group, which is *wider*.
pub async fn configured_run_context(
    config_store:  &GlobalConfigManager,
    registry_pool: &SqlitePool,
    key:           &str,
    user_id:       &str,
) -> Option<RunContext> {
    let configured = config_store
        .get(key)
        .await
        .ok()
        .flatten()
        .filter(|g| !g.is_empty());

    match configured {
        Some(group) => {
            let wanted = RunContext::with_security_group(Some(group));
            run_context::reconcile_group_for_user(registry_pool, user_id, Some(wanted)).await
        }
        None => run_context::role_default_run_context(registry_pool, user_id).await,
    }
}

/// Read an instance-wide boolean switch, defaulting to on.
pub async fn enabled_from_config(config_store: &GlobalConfigManager, key: &str) -> bool {
    match config_store.get(key).await {
        Ok(Some(v)) => v != "false",
        _           => true,
    }
}

/// Read an interval expressed in `unit_secs`-sized units, falling back to
/// `default_secs` when unset, unparseable or zero.
pub async fn interval_from_config(
    config_store: &GlobalConfigManager,
    key:          &str,
    unit_secs:    u64,
    default_secs: u64,
) -> u64 {
    if let Ok(Some(val)) = config_store.get(key).await {
        if let Ok(n) = val.trim().parse::<u64>() {
            if n > 0 {
                return n.saturating_mul(unit_secs);
            }
        }
    }
    default_secs
}

/// The on/off switch every system agent has.
pub fn enabled_property(key: &str, description: &str) -> ConfigProperty {
    ConfigProperty {
        key:           key.into(),
        name:          "Enabled".into(),
        description:   description.into(),
        property_type: PropertyType::Bool,
        default_value: Some("true".into()),
    }
}

/// The security-group picker every system agent has. The wording spells out the
/// per-user reconciliation, because an admin choosing a wide group here would
/// otherwise expect it to apply verbatim.
pub fn security_group_property(key: &str) -> ConfigProperty {
    ConfigProperty {
        key:           key.into(),
        name:          "Security group".into(),
        description:   "Tool permission group applied to each run. It is re-checked against each \
                        user's own role: a user whose role does not allow this group runs under \
                        their role's default group instead. Leave empty to always use the role \
                        default."
            .into(),
        property_type: PropertyType::SecurityGroup,
        default_value: None,
    }
}

/// Wrap the `notify` tool so the run log can report how many notifications a
/// pass actually produced, without the tool itself knowing it is being counted.
fn counting_notify(hub: Arc<ChatHub>, label: &str) -> (InterfaceTool, Arc<AtomicUsize>) {
    let inner   = crate::tools::notify::make_tool(hub, label);
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

/// Every system agent's config set must be owned by the agent, or its settings
/// silently land on the general Config page instead of its own tab.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tic_config_set_is_owned_by_tic() {
        let set = crate::tic::config_set();
        assert_eq!(set.owner.as_deref(), Some(crate::tic::TIC_AGENT));
    }

    #[test]
    fn lint_config_sets_are_owned_by_their_agents() {
        assert_eq!(
            memory_lint::private_config_set().owner.as_deref(),
            Some(memory_lint::PRIVATE_AGENT),
        );
        assert_eq!(
            memory_lint::shared_config_set().owner.as_deref(),
            Some(memory_lint::SHARED_AGENT),
        );
    }

    #[tokio::test]
    async fn registry_and_config_sets_agree() {
        // Constructing the agents touches no table — the pool is only a handle
        // they hold on to — so an empty database is enough here.
        let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
        let config = Arc::new(GlobalConfigManager::new(
            Arc::clone(&pool),
            Arc::new(core_api::system_bus::SystemEventBus::new()),
        ));

        let scheduled: Vec<&str> =
            registry(Default::default(), config, pool).iter().map(|a| a.id()).collect();
        let configured: Vec<String> = config_sets()
            .into_iter()
            .map(|s| s.owner.expect("a system agent's config set must be owned by it"))
            .collect();

        assert_eq!(
            scheduled, configured,
            "the scheduler's agents and the settings surface have drifted apart",
        );
    }

    #[test]
    fn every_agent_declares_its_interval_key_among_its_properties() {
        // The scheduler watches `interval_key()` for live changes; a key that is
        // not in the set is one nothing can ever edit.
        for (set, key) in [
            (crate::tic::config_set(), crate::tic::TIC_INTERVAL_MINUTES_KEY),
            (memory_lint::private_config_set(), memory_lint::PRIVATE_INTERVAL_DAYS_KEY),
            (memory_lint::shared_config_set(), memory_lint::SHARED_INTERVAL_DAYS_KEY),
        ] {
            assert!(
                set.properties.iter().any(|p| p.key == key),
                "`{key}` is watched by the scheduler but is not an editable property",
            );
        }
    }
}
