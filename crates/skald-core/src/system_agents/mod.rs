//! System agents — the background agents the instance runs on a user's behalf.
//!
//! A system agent runs without being asked. Event triage was the first, and
//! everything it needed turned out to be general: an on/off switch, an interval,
//! a security group reconciled against the user's own role, an ephemeral
//! session, and a run recorded in the user's own database. This module is that
//! shape, extracted, so a second agent is a [`SystemAgent`] impl and nothing
//! else — no timer of its own, no bookkeeping of its own, no scheduler of its own.
//!
//! **The unit of work is one agent for one user.** The instance-wide scheduler
//! (`skald::wiring::spawn_system_agents`) decides who and when; an agent decides
//! only what. That split is what made event triage per-user correct, and it is
//! why an agent never sees the user list.
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
//!    `start` for that agent — safe only because no two passes of one agent over
//!    one target are ever live at once. That used to be a property of the
//!    scheduler being a single sequential loop; since the **Run now** button it is
//!    enforced explicitly, by [`SystemAgents::claim`], which every starter goes
//!    through.

pub mod conversation_review;
pub mod memory_lint;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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
    /// One pass per **supervised subject**, run inside a supervisor's runtime.
    ///
    /// For work done *about* one person *for* another (`crate::db::supervision`).
    /// The two halves come apart here in a way neither other variant needs: the
    /// data read is the subject's, while the runtime doing the reading — the
    /// ephemeral session, the LLM turn, the run log — belongs to a supervisor.
    /// Which is the point: everything the pass leaves behind lands in the
    /// watcher's file, not the watched one's.
    ///
    /// Two consequences worth knowing before writing one:
    ///
    /// - **Due-ness is per subject and does not go through [`is_due`].** That
    ///   helper keys scheduler state by agent within one file, which would
    ///   collapse every subject sharing a supervisor into a single clock. These
    ///   agents answer scheduling themselves inside [`SystemAgent::has_work`],
    ///   against `crate::db::system_agent_coverage`.
    /// - **The subject need not be logged in**, as long as their database is not
    ///   encrypted (`UserManager::open_unencrypted`). An encrypted subject is
    ///   readable only while their own session is live — no key, no pass.
    PerSubject,
}

/// What one pass did, for the run log.
pub struct AgentOutcome {
    /// The ephemeral session the pass ran in, so the UI can link to it.
    pub session_id: Option<i64>,
    /// The agent's own counters. Never the contents of what it read.
    pub stats: serde_json::Value,
}

/// Who a [`AgentScope::PerSubject`] pass is *about*, when that is not the person
/// whose runtime it is running in.
#[derive(Clone, Copy)]
pub struct AgentSubject<'a> {
    pub user_id:  &'a str,
    pub username: &'a str,
    /// The subject's database, opened for reading. Not necessarily an unlocked
    /// session's pool — see `UserManager::open_unencrypted`.
    pub pool:     &'a SqlitePool,
}

/// One user's runtime, unpacked from their `UserContext` by the scheduler.
///
/// The four leading fields always describe the runtime **the pass executes in**,
/// which for every scope but [`AgentScope::PerSubject`] is also whom the pass is
/// about. Keeping that meaning fixed is what lets `run_ephemeral_turn` stay
/// unaware of the distinction: it always writes into the acting runtime.
#[derive(Clone, Copy)]
pub struct AgentRunCtx<'a> {
    pub user_id:  &'a str,
    /// The user's own (unlocked) database.
    pub pool:     &'a SqlitePool,
    pub sessions: &'a Arc<ChatSessionManager>,
    pub hub:      &'a Arc<ChatHub>,
    /// Set only for [`AgentScope::PerSubject`]: the person being looked at.
    pub subject:  Option<AgentSubject<'a>>,
    /// The `system_agent_runs` row this pass is being recorded under, filled in by
    /// [`run_and_record`] before it calls [`SystemAgent::run`]. Lets an agent that
    /// produces a durable artefact point back at the run that made it — across
    /// files, where a foreign key cannot reach.
    pub run_id:   Option<i64>,
}

#[async_trait]
pub trait SystemAgent: Send + Sync {
    /// Directory name under `agents/`, and the `agent_id` of its rows.
    fn id(&self) -> &'static str;

    fn scope(&self) -> AgentScope;

    /// The settings shown on this agent's tab of the System agents page. Must be
    /// `owned_by(self.id())`, or it lands on the general Config page instead.
    fn config_set(&self) -> ConfigSet;

    /// The config key that governs this agent's cadence. The scheduler watches it
    /// so a change in the UI reschedules without a restart.
    ///
    /// Usually the interval itself; for an agent that runs at a fixed time of day
    /// it is the hour, which is the key that moves the next pass just the same.
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
    event_triage_config: crate::config::EventTriageConfig,
    config_store:        Arc<GlobalConfigManager>,
    registry_pool:       Arc<SqlitePool>,
    system_bus:          Arc<core_api::system_bus::SystemEventBus>,
) -> Vec<Arc<dyn SystemAgent>> {
    vec![
        crate::event_triage::EventTriageManager::new(
            event_triage_config,
            Arc::clone(&config_store),
            Arc::clone(&registry_pool),
        ),
        memory_lint::MemoryLintAgent::private(
            Arc::clone(&config_store),
            Arc::clone(&registry_pool),
        ),
        memory_lint::MemoryLintAgent::shared(
            Arc::clone(&config_store),
            Arc::clone(&registry_pool),
        ),
        conversation_review::ConversationReviewAgent::new(config_store, registry_pool, system_bus),
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
        crate::event_triage::config_set(),
        memory_lint::private_config_set(),
        memory_lint::shared_config_set(),
        conversation_review::config_set(),
    ]
}

/// The instance's agents, built once and shared by everything that can start a
/// pass.
///
/// There are two such things now — the scheduler and the **Run now** button — and
/// that is the whole reason this type exists. As long as the scheduler was the
/// only starter, "sequential and single-instance" was a property of one loop and
/// needed no enforcement; a manual trigger breaks it, and the breakage is not
/// cosmetic: [`system_agent_runs::start`] sweeps any leftover `running` row of the
/// same agent to `failed` before inserting, so a second pass beginning while the
/// first is alive would mark a perfectly healthy run as *interrupted* and then
/// duplicate its work.
///
/// So the invariant moves out of the loop and into [`claim`](Self::claim), which
/// both paths go through. It is a plain [`std::sync::Mutex`]: nothing is awaited
/// while it is held, and the guard has to be released from [`Drop`], where an
/// async lock could not be.
pub struct SystemAgents {
    agents: Vec<Arc<dyn SystemAgent>>,
    /// `(agent_id, target)` of every pass currently in flight.
    active: Mutex<HashSet<(&'static str, String)>>,
}

/// The target of an [`AgentScope::Instance`] pass. Not a user id: the shared
/// store belongs to nobody, so two people asking for it at once must still be one
/// pass, not one each.
const INSTANCE_TARGET: &str = "@instance";

impl SystemAgents {
    pub fn new(
        event_triage_config: crate::config::EventTriageConfig,
        config_store:        Arc<GlobalConfigManager>,
        registry_pool:       Arc<SqlitePool>,
        system_bus:          Arc<core_api::system_bus::SystemEventBus>,
    ) -> Arc<Self> {
        Arc::new(Self {
            agents: registry(event_triage_config, config_store, registry_pool, system_bus),
            active: Mutex::new(HashSet::new()),
        })
    }

    /// Every agent, in pass order.
    pub fn all(&self) -> &[Arc<dyn SystemAgent>] { &self.agents }

    pub fn get(&self, agent_id: &str) -> Option<&Arc<dyn SystemAgent>> {
        self.agents.iter().find(|a| a.id() == agent_id)
    }

    /// What a pass of `agent` acting as `user_id` is *about* — the key passes are
    /// serialised on. Per-user work is per user; instance work is one thing no
    /// matter who runs it.
    pub fn target_of(agent: &dyn SystemAgent, user_id: &str) -> String {
        match agent.scope() {
            AgentScope::Instance => INSTANCE_TARGET.to_string(),
            _                    => user_id.to_string(),
        }
    }

    /// Claim the right to run `agent` over `target`. `None` means a pass is
    /// already in flight and this one must not start — held until the returned
    /// [`RunClaim`] is dropped, including on panic or early return.
    pub fn claim(self: &Arc<Self>, agent_id: &'static str, target: &str) -> Option<RunClaim> {
        let key = (agent_id, target.to_string());
        let mut active = self.active.lock().unwrap();
        if !active.insert(key.clone()) {
            return None;
        }
        Some(RunClaim { owner: Arc::clone(self), key })
    }
}

/// A live claim on one `(agent, target)` pair. Releasing it is [`Drop`]'s job so
/// that no early return can leak the slot and wedge an agent for the rest of the
/// process's life.
pub struct RunClaim {
    owner: Arc<SystemAgents>,
    key:   (&'static str, String),
}

impl Drop for RunClaim {
    fn drop(&mut self) {
        if let Ok(mut active) = self.owner.active.lock() {
            active.remove(&self.key);
        }
    }
}

/// What a manual trigger did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualRun {
    /// The pass is running in the background; the run log is where it reports.
    Started,
    /// [`SystemAgent::has_work`] said there was nothing to look at, so no run was
    /// opened — the same silence a scheduled idle pass leaves behind. Answered
    /// before spawning anything, so the button can say so straight away instead
    /// of leaving the person watching a log that will never gain a row.
    NothingToDo,
}

/// Why a manual trigger could not start. Each variant is a different thing to
/// tell the person who pressed the button, which is why this is not one string.
#[derive(Debug)]
pub enum ManualRunError {
    UnknownAgent,
    /// [`AgentScope::PerSubject`]: the pass is about somebody else, so "run it for
    /// me" has no meaning. Supervisors triggering a review of one subject would be
    /// a different button, with a subject to pick.
    Unsupported,
    /// Switched off instance-wide. Deliberately **not** overridden by a manual
    /// trigger, unlike due-ness: the interval says *when*, and a human asking is a
    /// good enough answer to that — the switch says *whether*, and only the admin
    /// who set it gets to answer that one.
    Disabled,
    AlreadyRunning,
    /// The caller's database is locked (§9), so there is nothing to read and
    /// nowhere to record the run.
    Locked,
    /// `has_work` itself failed — the pass never started.
    Failed(anyhow::Error),
}

impl fmt::Display for ManualRunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownAgent   => write!(f, "no such system agent"),
            Self::Unsupported    => write!(f, "this agent runs about another person, not about you, \
                                               and cannot be started by hand"),
            Self::Disabled       => write!(f, "this agent is disabled for the whole instance"),
            Self::AlreadyRunning => write!(f, "a run of this agent is already in progress"),
            Self::Locked         => write!(f, "session expired — please log in again"),
            Self::Failed(e)      => write!(f, "{e}"),
        }
    }
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
    //
    // Skipped for a per-subject pass, and not as an optimisation: that state is
    // keyed by agent inside one file, so several subjects sharing a supervisor
    // would overwrite each other's row and the first subject of the evening would
    // silently stand for all of them. Those agents keep their own per-subject
    // watermark (`db::system_agent_coverage`) and are gated by `has_work` alone.
    if agent.scope() != AgentScope::PerSubject {
        if let Err(e) = system_agent_state::mark_attempt(ctx.pool, agent.id()).await {
            warn!(agent = agent.id(), user = %ctx.user_id, error = %e,
                  "system-agents: could not record the attempt");
        }
    }

    // Step 2 — nothing to do leaves no trace.
    if !agent.has_work(ctx).await? {
        return Ok(None);
    }

    // Step 3 — open the row, then work. The pass runs with the row's id in hand,
    // so whatever it produces can name the run that produced it.
    let run_id  = system_agent_runs::start(ctx.pool, agent.id()).await?;
    let started = Instant::now();
    let ctx     = &AgentRunCtx { run_id: Some(run_id), ..*ctx };

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
    agent_id:      &str,
    source:        &str,
    prompt:        &str,
    run_context:   Option<&RunContext>,
    notify_label:  &str,
    // `<!-- KEY -->` placeholders in the agent's `AGENT.md`, resolved for this
    // pass. The two the system context resolves by itself (`__USER_PROFILE__`,
    // `__SHARED_FOLDERS__`) describe the *session owner*, which for a pass about
    // somebody else is the wrong person — so an agent that needs the subject's
    // details supplies them here, under its own key, rather than being handed a
    // profile that silently means the runtime's owner.
    substitutions: HashMap<String, String>,
    ctx:           &AgentRunCtx<'_>,
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
            substitutions,
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
    fn event_triage_config_set_is_owned_by_event_triage() {
        let set = crate::event_triage::config_set();
        assert_eq!(set.owner.as_deref(), Some(crate::event_triage::EVENT_TRIAGE_AGENT));
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
        let bus    = Arc::new(core_api::system_bus::SystemEventBus::new());
        let config = Arc::new(GlobalConfigManager::new(Arc::clone(&pool), Arc::clone(&bus)));

        let scheduled: Vec<&str> = registry(Default::default(), config, pool, bus)
            .iter().map(|a| a.id()).collect();
        let configured: Vec<String> = config_sets()
            .into_iter()
            .map(|s| s.owner.expect("a system agent's config set must be owned by it"))
            .collect();

        assert_eq!(
            scheduled, configured,
            "the scheduler's agents and the settings surface have drifted apart",
        );
    }

    /// Constructing the agents touches no table — the pool is only a handle they
    /// hold on to — so an empty database is enough.
    async fn test_agents() -> Arc<SystemAgents> {
        let pool = Arc::new(SqlitePool::connect("sqlite::memory:").await.unwrap());
        let bus  = Arc::new(core_api::system_bus::SystemEventBus::new());
        let cfg  = Arc::new(GlobalConfigManager::new(Arc::clone(&pool), Arc::clone(&bus)));
        SystemAgents::new(Default::default(), cfg, pool, bus)
    }

    #[tokio::test]
    async fn a_claim_is_exclusive_per_target_and_ends_with_its_guard() {
        let agents = test_agents().await;

        let alice = agents.claim(memory_lint::PRIVATE_AGENT, "alice").expect("nothing in flight");
        // The scheduler waking up mid-manual-run, or a second click.
        assert!(agents.claim(memory_lint::PRIVATE_AGENT, "alice").is_none());
        // Somebody else's pass of the same agent is unrelated work.
        assert!(agents.claim(memory_lint::PRIVATE_AGENT, "bob").is_some());
        // As is the same person's pass of a different agent.
        assert!(agents.claim(memory_lint::SHARED_AGENT, "alice").is_some());

        drop(alice);
        assert!(agents.claim(memory_lint::PRIVATE_AGENT, "alice").is_some());
    }

    #[tokio::test]
    async fn an_instance_agent_is_one_slot_whoever_runs_it() {
        let agents = test_agents().await;

        // Two members pressing "Run now" on the shared store must be one pass, not
        // one each — the store they read is the same one.
        let shared = agents.get(memory_lint::SHARED_AGENT).unwrap();
        assert_eq!(
            SystemAgents::target_of(shared.as_ref(), "alice"),
            SystemAgents::target_of(shared.as_ref(), "bob"),
        );

        // A per-user agent is the opposite: two people, two independent passes.
        let private = agents.get(memory_lint::PRIVATE_AGENT).unwrap();
        assert_ne!(
            SystemAgents::target_of(private.as_ref(), "alice"),
            SystemAgents::target_of(private.as_ref(), "bob"),
        );
    }

    #[test]
    fn every_agent_declares_its_interval_key_among_its_properties() {
        // The scheduler watches `interval_key()` for live changes; a key that is
        // not in the set is one nothing can ever edit.
        for (set, key) in [
            (crate::event_triage::config_set(), crate::event_triage::EVENT_TRIAGE_INTERVAL_MINUTES_KEY),
            (memory_lint::private_config_set(), memory_lint::PRIVATE_INTERVAL_DAYS_KEY),
            (memory_lint::shared_config_set(), memory_lint::SHARED_INTERVAL_DAYS_KEY),
            (conversation_review::config_set(), conversation_review::RUN_AT_HOUR_KEY),
        ] {
            assert!(
                set.properties.iter().any(|p| p.key == key),
                "`{key}` is watched by the scheduler but is not an editable property",
            );
        }
    }
}
