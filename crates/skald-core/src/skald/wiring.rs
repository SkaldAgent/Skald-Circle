//! Post-construction wiring: the `OnceLock` cycle-breakers and the background-task
//! spawns, each concentrated in one readable place instead of being scattered
//! through the constructor.
//!
//! Owner-bound background loops (cron, session-cancel, ticket-listener) have
//! moved per-user into `UserContextFactory::build`. What remains here are the
//! instance-wide tasks: LLM-log cleanup on the registry pool, MCP server
//! initialization, and the two that need the finished `Arc<Skald>` and are
//! therefore spawned separately, after construction — the user-lifecycle
//! reconciler and the system-agent scheduler.

use std::sync::Arc;
use std::time::Duration;

use core_api::system_bus::{RecvError, SystemEvent};
use tracing::{info, warn};

use crate::config::CoreConfig;
use crate::elicitation::ElicitationBridge;
use crate::system_agents::{self, AgentRunCtx, AgentScope, SystemAgent};

use super::bundles::{Conversation, Integrations, Interaction, Tasks};
use super::runtime::Runtime;

/// Resolves the construction cycles (`cron ↔ session ↔ hub`, `ticket → cron`,
/// `mcp → elicitation`) via the managers' `OnceLock` setters.
///
/// These wire the **global** bundles, which are transitional and will be removed
/// once all call-sites are per-user (Phase 6). They remain constructed so any
/// not-yet-migrated accessor does not panic on a `None` OnceLock.
pub(super) fn wire(
    tasks: &Tasks,
    conversation: &Conversation,
    integrations: &Integrations,
    interaction: &Interaction,
) {
    tasks.cron.set_session(Arc::clone(&conversation.manager));
    tasks.cron.set_hub(Arc::clone(&conversation.chat_hub));
    tasks.cron.set_self_arc(Arc::clone(&tasks.cron));
    conversation.chat_hub.set_task_mgr(Arc::clone(&tasks.cron));
    integrations.mcp.set_elicitation_handler(ElicitationBridge::new(Arc::clone(&interaction.elicitation)));
    info!("ChatHub initialised");
}

/// Spawns the instance-wide background tasks.
///
/// Owner-bound loops (cron, session-cancel, ticket-listener) are **not** spawned
/// here — they run per-user inside `UserContext`. Session cancellation is handled
/// directly by the API handlers (which have `AuthUser` and resolve the per-user
/// context). The system-agent scheduler needs the finished instance and lives in
/// [`spawn_system_agents`].
pub(super) fn spawn_background(
    rt: &Runtime,
    _tasks: &Tasks,
    _conversation: &Conversation,
    integrations: &Integrations,
    config: &CoreConfig,
) {
    // LLM request-log retention/cleanup — first run 1 min after startup, then 12h.
    if let Some(cfg) = config.llm.requests_log.clone().filter(|r| r.enabled) {
        rt.supervisor.adopt_one(
            "llm-log-cleanup",
            crate::db::llm_requests::cleanup::spawn(
                Arc::clone(&rt.db),
                cfg,
                rt.shutdown_token.clone(),
            ),
        );
    }

    // MCP servers connect in the background. `initialize()` does not itself observe
    // the cancellation token, so race it against shutdown: on cancel the task exits
    // promptly (dropping the in-flight connection attempts) instead of blocking the
    // shutdown join until the deadline.
    {
        let mcp = Arc::clone(&integrations.mcp);
        let sd = rt.shutdown_token.clone();
        rt.supervisor.spawn("mcp-init", async move {
            tokio::select! {
                _ = sd.cancelled() => {}
                _ = mcp.initialize() => {}
            }
        });
    }

    // Skills freshness for edits made by hand on the box (blueprint §8.2). The
    // in-process writers invalidate directly; this watches the two trees and
    // announces `SkillsChanged` only when the digest gate says the index moved.
    rt.supervisor.adopt_one(
        "skills-watch",
        crate::skills::watch::spawn(Arc::clone(&rt.system_bus), rt.shutdown_token.clone()),
    );
}

/// Spawns the **user-lifecycle reconciler** — the single subscriber that turns
/// user and membership events into container work (blueprint §6).
///
/// Producers (the Users admin page, the setup wizard, the shared-folder and
/// project membership endpoints) only announce *what changed*; none of them
/// reaches into [`ContainerManager`](crate::container::ContainerManager). That
/// is the point of routing this through the bus rather than calling the manager
/// from each handler: a future endpoint that grants membership cannot forget to
/// remount, because remounting was never its job.
///
/// Every reaction is **best-effort by contract**: the row is already committed
/// when the event fires, so a Docker hiccup is logged, never surfaced to the
/// caller — the state settles at the user's next login or at boot
/// reconciliation. Events are handled **sequentially**, which also serialises
/// concurrent `docker` operations on the same container.
///
/// Spawned after `Skald` is fully built (like `set_skald`) because
/// [`Skald::refresh_user_mounts`] is an accessor on the finished instance. The
/// back-reference is [`std::sync::Weak`], so this task never keeps `Skald` alive.
pub(super) fn spawn_user_lifecycle(skald: &Arc<super::Skald>) {
    let weak     = Arc::downgrade(skald);
    let shutdown = skald.rt.shutdown_token.clone();
    let mut rx   = skald.rt.system_bus.subscribe();

    skald.rt.supervisor.spawn("user-lifecycle", async move {
        loop {
            let event = tokio::select! {
                _ = shutdown.cancelled() => break,
                event = rx.recv() => match event {
                    Ok(e) => e,
                    // A dropped event costs a stale container until the user's next
                    // login/boot — never a lost row, since the DB write came first.
                    Err(RecvError::Lagged(n)) => {
                        warn!(n, "user-lifecycle: system_bus lagged; container state may be stale until next login/boot");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                },
            };

            let Some(skald) = weak.upgrade() else { break };
            match event {
                SystemEvent::UserCreated { user_id } => {
                    if let Err(e) = skald.container().ensure(&user_id).await {
                        warn!(user = %user_id, error = %e,
                            "user-lifecycle: failed to provision container (retried at next boot)");
                    }
                    // After the container, never before: the runtime snapshots the
                    // user's fs and starts their per-user MCP servers inside it.
                    start_runtime_if_unencrypted(&skald, &user_id).await;
                }
                SystemEvent::UserDeleted { user_id } => {
                    if let Err(e) = skald.container().remove(&user_id).await {
                        warn!(user = %user_id, error = %e,
                            "user-lifecycle: failed to remove container");
                    }
                }
                // Match boot reconciliation, which keeps a container only for active
                // users. The user's live runtime is already gone by now — the handler
                // revoked it synchronously before emitting.
                SystemEvent::UserActiveChanged { user_id, active } => {
                    let result = if active {
                        skald.container().ensure(&user_id).await
                    } else {
                        skald.container().stop(&user_id).await
                    };
                    if let Err(e) = result {
                        warn!(user = %user_id, active, error = %e,
                            "user-lifecycle: failed to apply active-state change to container");
                    }
                    if active {
                        start_runtime_if_unencrypted(&skald, &user_id).await;
                    }
                }
                SystemEvent::UserMountsChanged { user_id } => {
                    if let Err(e) = skald.refresh_user_mounts(&user_id).await {
                        warn!(user = %user_id, error = %e,
                            "user-lifecycle: remount failed (settles at next login/boot)");
                    }
                }
                // Both are pure appearance/metadata refreshes across live users —
                // they widen or re-sync what is visible, never narrow it, which is
                // what makes them safe to hand to a best-effort bus.
                SystemEvent::McpGlobalServersChanged => {
                    skald.refresh_global_mcp_access().await;
                }
                SystemEvent::ConnectorReinstalled { catalog_name } => {
                    skald.refresh_connector_after_reinstall(&catalog_name).await;
                }
                _ => {}
            }
        }
        info!("user-lifecycle: reconciler stopped");
    });
}

/// Gives a user who has just appeared (created, or reactivated) the same
/// treatment boot gives everyone: if their database has no key, unlock it and
/// start their runtime now rather than at their first login (see
/// [`spawn_unlocked_user_runtimes`]).
///
/// Reconciliation, hence the bus: a lost event costs a user whose channels and
/// cron stay asleep until the next restart or login, never a wrong grant — this
/// can only open a file that is already readable by this process, and never
/// touches authentication. An encrypted or inactive user is refused inside the
/// manager, so the outcome is a debug line, not a failure.
async fn start_runtime_if_unencrypted(skald: &Arc<super::Skald>, user_id: &str) {
    if let Err(e) = skald.users().unlock_unencrypted(user_id).await {
        tracing::debug!(user = %user_id, reason = %e, "user-lifecycle: database not auto-unlocked");
        return;
    }
    if skald.user_context(user_id).await.is_none() {
        warn!(user = %user_id, "user-lifecycle: could not start runtime (retried at next boot/login)");
    }
}

/// Builds the per-user runtime of every database boot unlocked, so an instance
/// whose members are unencrypted comes up **working** rather than merely
/// unlocked.
///
/// Unlocking a pool only makes the data readable; what actually runs a person's
/// scheduled jobs, delivers their notifications and feeds their channel plugins
/// is their [`UserContext`](super::UserContext) — cron loop, hub, notify queue
/// and per-user MCP runtime all live there, and it is built lazily on first use.
/// Left lazy, a restart meant a cron job fired only once somebody had opened the
/// web UI, which for an unattended box is indistinguishable from it not firing.
///
/// **Background, not part of `Skald::new`**: a build starts that user's MCP
/// servers inside their container, so doing this inline would hold the HTTP
/// listener behind every member's connector startup. Sequential for the same
/// reason `reconcile_all` is — these are docker operations, and the registry
/// serialises builds anyway.
///
/// Encrypted users are absent by construction: they hold no unlocked pool, so
/// their runtime is still built by their login, as §9 requires.
pub(super) fn spawn_unlocked_user_runtimes(skald: &Arc<super::Skald>) {
    let weak     = Arc::downgrade(skald);
    let shutdown = skald.rt.shutdown_token.clone();

    skald.rt.supervisor.spawn("user-runtimes-boot", async move {
        let users = {
            let Some(skald) = weak.upgrade() else { return };
            match skald.users().list().await {
                Ok(u) => u,
                Err(e) => {
                    warn!(error = %e, "boot: could not list users to start their runtimes");
                    return;
                }
            }
        };

        let mut started = 0usize;
        for user in users.iter().filter(|u| u.active) {
            if shutdown.is_cancelled() {
                return;
            }
            // Whatever boot unlocked — never a decision re-derived from the row,
            // so this cannot widen past what `unlock_all_unencrypted` allowed.
            let Some(skald) = weak.upgrade() else { return };
            if !skald.users().is_unlocked(&user.id) {
                continue;
            }
            match skald.user_context(&user.id).await {
                Some(_) => started += 1,
                None => warn!(user = %user.id,
                    "boot: failed to start user runtime (retried at their next login)"),
            }
        }

        if started > 0 {
            info!(started, "boot: per-user runtimes started without a login");
        }
    });
}

/// Spawns the **skills-freshness reactor** — the subscriber that turns a
/// `SkillsChanged` announcement into a prompt-prefix invalidation (blueprint
/// §8.3).
///
/// Why the bus at all, when the skill tools call `invalidate_prompt_prefix`
/// directly: the watcher exists for a writer *outside* the process (a hand
/// edit on the box), and its consumers live in different places — the per-user
/// loop runtimes here, a UI refresh later. A direct call would make the
/// watcher hold `Skald`, which it deliberately does not. Best-effort by
/// contract, and honestly so: a lost event costs a stale skill index for the
/// twenty minutes of the prefix TTL, never a wrong answer.
pub(super) fn spawn_skills_freshness(skald: &Arc<super::Skald>) {
    let weak     = Arc::downgrade(skald);
    let shutdown = skald.rt.shutdown_token.clone();
    let mut rx   = skald.rt.system_bus.subscribe();

    skald.rt.supervisor.spawn("skills-freshness", async move {
        loop {
            let event = tokio::select! {
                _ = shutdown.cancelled() => break,
                event = rx.recv() => match event {
                    Ok(e) => e,
                    Err(RecvError::Lagged(n)) => {
                        warn!(n, "skills-freshness: system_bus lagged; a skill index may be stale until the prefix TTL");
                        continue;
                    }
                    Err(RecvError::Closed) => break,
                },
            };

            let SystemEvent::SkillsChanged { scope } = event else { continue };
            let Some(skald) = weak.upgrade() else { break };
            let scope = match scope {
                core_api::system_bus::SkillScope::Global   => crate::skills::PromptScope::Everyone,
                core_api::system_bus::SkillScope::User(id) => crate::skills::PromptScope::User(id),
            };
            skald.invalidate_prompt_prefix(scope).await;
        }
        info!("skills-freshness: reactor stopped");
    });
}

/// Spawns the **system-agent scheduler** — the one instance-wide timer behind
/// every background agent nobody asked for (event triage, the two memory lints).
///
/// **One loop for all of them.** The agents differ by three orders of magnitude
/// in cadence — event triage every few minutes, a lint every week — which is
/// exactly the case that tempts a second loop. It stays one because the wake-up
/// decides nothing: [`base_tick`] only picks how often to *look*, and whether an
/// agent actually runs for a given user is [`system_agents::is_due`] against
/// state in that user's own database. Adding an agent therefore adds a registry
/// entry, never a task.
///
/// **Due-ness is persisted, not counted from boot.** An in-memory deadline is
/// fine at event triage's scale but silently breaks a weekly agent: every restart
/// re-arms it, so on a machine rebooted every few days it would never fire once.
/// Reading the last attempt from `system_agent_state` makes a long interval
/// survive restarts, and has the pleasant side effect that a user who logs in
/// after a long absence is picked up on the next pass rather than a week later.
///
/// A user whose database is still locked is **skipped**, and that is the normal
/// case rather than an error: the pool is the unlock token (§9), so a user who
/// has not logged in since the last restart has no readable events, no session
/// store, and no place to record the skip. It is logged at INFO and the pass
/// moves on; their events keep accumulating and are picked up by the first pass
/// after they log in.
///
/// Spawned after `Skald` is fully built, like [`spawn_user_lifecycle`] and for
/// the same reason: it resolves each user's runtime through `Skald::user_context`.
/// The back-reference is [`std::sync::Weak`].
pub(super) fn spawn_system_agents(skald: &Arc<super::Skald>) {
    let weak       = Arc::downgrade(skald);
    let shutdown   = skald.rt.shutdown_token.clone();
    let mut sys_rx = skald.rt.system_bus.subscribe();

    // Adding an agent is one line in `system_agents::registry` plus a
    // `SystemAgent` impl — no loop of its own, which is the whole point: a second
    // scheduler would be a fourth global bus in disguise. The list is the
    // instance's (`Skald::system_agents`), not this loop's: the "Run now" button
    // starts the very same agents, and both go through one in-flight guard.
    let agents: Vec<Arc<dyn SystemAgent>> = skald.system_agents.all().to_vec();

    // Interval keys, so a change in the UI cuts the current wait short for
    // whichever agent it belongs to.
    let interval_keys: Vec<&'static str> = agents.iter().map(|a| a.interval_key()).collect();

    skald.rt.supervisor.spawn("system-agents", async move {
        info!(agents = agents.len(), "system-agents: scheduler started");

        'outer: loop {
            let wait = base_tick(&agents).await;
            let deadline = tokio::time::sleep(wait);
            tokio::pin!(deadline);

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break 'outer,
                    _ = &mut deadline       => break,
                    ev = sys_rx.recv() => match ev {
                        Ok(SystemEvent::ConfigKeyUpdated { key, .. })
                            if interval_keys.contains(&key.as_str()) =>
                        {
                            info!(%key, "system-agents: interval changed, rescheduling");
                            continue 'outer;
                        }
                        Err(RecvError::Closed) => break 'outer,
                        _ => {}
                    },
                }
            }

            let Some(skald) = weak.upgrade() else { break };
            agents_pass(&skald, &agents).await;
        }

        info!("system-agents: scheduler stopped");
    });
}

/// How long to sleep between passes: the shortest interval any enabled agent
/// asks for, clamped.
///
/// The wake-up itself decides nothing — every agent is gated per user by
/// [`system_agents::is_due`] against persisted state — so this only has to be
/// fine-grained enough not to delay the most impatient agent, and coarse enough
/// not to spin. The floor keeps a misconfigured one-minute interval from turning
/// into a busy loop; the ceiling keeps a box that runs only weekly agents from
/// sleeping so long that a freshly changed setting takes hours to be noticed.
async fn base_tick(agents: &[Arc<dyn SystemAgent>]) -> Duration {
    const FLOOR_SECS: u64 = 60;
    const CEIL_SECS:  u64 = 15 * 60;

    let mut shortest = CEIL_SECS;
    for agent in agents {
        if agent.is_enabled().await {
            shortest = shortest.min(agent.interval_secs().await);
        }
    }
    Duration::from_secs(shortest.clamp(FLOOR_SECS, CEIL_SECS))
}

/// One pass over every agent, sequentially.
///
/// Sequential on purpose, and at two levels: agents one after another, and
/// within a per-user agent, users one after another. A pass is N container
/// round-trips and N LLM calls, nobody is waiting on it, and running them
/// concurrently would only spike the box every interval. It is also what makes
/// the `running` row of a crashed pass safe to sweep — no other run of the same
/// agent can be live.
async fn agents_pass(skald: &Arc<super::Skald>, agents: &[Arc<dyn SystemAgent>]) {
    for agent in agents {
        if skald.rt.shutdown_token.is_cancelled() {
            return;
        }
        // Re-read per pass, so disabling an agent takes effect without a restart.
        if !agent.is_enabled().await {
            continue;
        }
        match agent.scope() {
            AgentScope::PerUser    => per_user_pass(skald, agent.as_ref()).await,
            AgentScope::Instance   => instance_pass(skald, agent.as_ref()).await,
            AgentScope::PerSubject => subject_pass(skald, agent.as_ref()).await,
        }
    }
}

/// Run `agent` for each active user whose database is unlocked and who is due.
async fn per_user_pass(skald: &Arc<super::Skald>, agent: &dyn SystemAgent) {
    let users = match skald.users().list().await {
        Ok(u) => u,
        Err(e) => {
            warn!(agent = agent.id(), error = %e,
                  "system-agents: cannot list users, skipping this pass");
            return;
        }
    };

    for user in users.into_iter().filter(|u| u.active) {
        if skald.rt.shutdown_token.is_cancelled() {
            return;
        }
        run_one(skald, agent, &user.id, &user.username).await;
    }
}

/// Run an instance-scoped `agent` once, as the admin.
///
/// The first active admin who is unlocked wins; ordering is `users::list`'s, so
/// the choice is stable across passes rather than racing between two admins. If
/// none has logged in since the last restart the pass is skipped exactly like a
/// locked user's — it settles at the next login.
async fn instance_pass(skald: &Arc<super::Skald>, agent: &dyn SystemAgent) {
    let users = match skald.users().list().await {
        Ok(u) => u,
        Err(e) => {
            warn!(agent = agent.id(), error = %e,
                  "system-agents: cannot list users, skipping this pass");
            return;
        }
    };

    let admins = users
        .into_iter()
        .filter(|u| u.active && u.role_id == crate::db::roles::ADMIN_ROLE_ID);

    for admin in admins {
        if skald.users().is_unlocked(&admin.id) {
            run_one(skald, agent, &admin.id, &admin.username).await;
            return;
        }
    }

    info!(
        agent = agent.id(),
        "system-agents: skipped — no admin has logged in since the last restart, \
         so the instance-wide pass has no runtime to run in",
    );
}

/// Run `agent` once per supervised subject, each pass inside a supervisor's
/// runtime.
///
/// Three properties, and each one is a decision rather than a detail:
///
/// - **The iteration is over subjects, not supervisors.** Two parents watching
///   the same child must produce one review of that child, not two. Whichever of
///   them is available lends their runtime; the report is filed against the
///   subject and every supervisor reads the same row.
/// - **The subject does not need to be logged in.** `open_unencrypted` opens
///   their file directly when it has no key, which is what makes a 4am pass
///   possible at all — nobody is at a keyboard then. An encrypted subject has no
///   such door and is reviewed only while their own session is live.
/// - **Due-ness is not checked here.** Unlike the other two passes, it is per
///   subject and lives in `system_agent_coverage`; the agent answers it inside
///   `has_work`. See [`AgentScope::PerSubject`].
async fn subject_pass(skald: &Arc<super::Skald>, agent: &dyn SystemAgent) {
    let subjects = match crate::db::supervision::subjects(&skald.rt.db).await {
        Ok(s) => s,
        Err(e) => {
            warn!(agent = agent.id(), error = %e,
                  "system-agents: cannot read the supervision edges, skipping this pass");
            return;
        }
    };

    for subject_id in subjects {
        if skald.rt.shutdown_token.is_cancelled() {
            return;
        }

        let subject = match skald.users().get(&subject_id).await {
            Ok(Some(u)) if u.active => u,
            Ok(_)  => continue, // deleted or deactivated: nothing to review
            Err(e) => {
                warn!(agent = agent.id(), user = %subject_id, error = %e,
                      "system-agents: cannot read the subject, skipping them");
                continue;
            }
        };

        // Their database, without asking them to be present — as long as it has
        // no key. A refusal here is the honest case, not a failure: an encrypted
        // person cannot be read while they are away, by anyone.
        let subject_pool = match skald.users().open_unencrypted(&subject_id).await {
            Ok(p)  => p,
            Err(e) => {
                info!(agent = agent.id(), user = %subject_id, reason = %e,
                      "system-agents: skipped — the subject's database cannot be read right now");
                continue;
            }
        };

        // Somebody entitled to the result has to lend a runtime for the work to
        // happen in. First unlocked supervisor wins, in the edge's stable order.
        let Some(host) = first_unlocked_supervisor(skald, &subject_id).await else {
            info!(agent = agent.id(), user = %subject_id,
                  "system-agents: skipped — none of this person's supervisors has logged in \
                   since the last restart, so the pass has no runtime to run in");
            continue;
        };

        let Some(ctx) = skald.user_context(&host).await else {
            warn!(agent = agent.id(), supervisor = %host,
                  "system-agents: skipped — could not resolve the supervisor's runtime");
            continue;
        };

        // Keyed on the **subject**, not the supervisor who lends the runtime: the
        // pass is about them, and two supervisors must not review one person twice.
        let Some(_claim) = skald.system_agents.claim(agent.id(), &subject_id) else {
            info!(agent = agent.id(), user = %subject_id,
                  "system-agents: skipped — a review of this person is already in progress");
            continue;
        };

        let run_ctx = AgentRunCtx {
            user_id:  &host,
            pool:     &ctx.pool,
            sessions: &ctx.sessions,
            hub:      &ctx.chat_hub,
            subject:  Some(system_agents::AgentSubject {
                user_id:  &subject_id,
                username: &subject.username,
                pool:     &subject_pool,
            }),
            run_id:   None,
        };

        // One subject's failure must not end the pass for everyone after them.
        if let Err(e) = system_agents::run_and_record(agent, &run_ctx).await {
            warn!(agent = agent.id(), user = %subject_id, error = %e,
                  "system-agents: pass failed");
        }
    }
}

/// The first supervisor of `subject` whose runtime is live, in the edge's stable
/// order — so the same one is picked pass after pass rather than alternating.
async fn first_unlocked_supervisor(skald: &Arc<super::Skald>, subject: &str) -> Option<String> {
    let supervisors = crate::db::supervision::supervisors_of(&skald.rt.db, subject)
        .await
        .unwrap_or_default();
    supervisors.into_iter().find(|s| skald.users().is_unlocked(s))
}

/// The common tail: skip a locked user, resolve their runtime, check due-ness,
/// run and record.
async fn run_one(
    skald:    &Arc<super::Skald>,
    agent:    &dyn SystemAgent,
    user_id:  &str,
    username: &str,
) {
    // A locked user is the normal case, not an error: the pool is the unlock
    // token (§9), so someone who has not logged in since the last restart has
    // nothing readable — and no place to record the skip, since the only file
    // that could hold it is the one we cannot open. Hence a log line and nothing
    // else; their next login picks it up.
    if !skald.users().is_unlocked(user_id) {
        info!(
            agent = agent.id(), user = %user_id, %username,
            "system-agents: skipped — the user's database is still encrypted \
             (not logged in since the last restart)",
        );
        return;
    }

    // Unlocked, so this resolves (and is normally already live from their login).
    let Some(ctx) = skald.user_context(user_id).await else {
        warn!(agent = agent.id(), user = %user_id,
              "system-agents: skipped — could not resolve the user's runtime");
        return;
    };

    if !system_agents::is_due(agent, &ctx.pool).await {
        return;
    }

    // Held for the whole pass. The scheduler alone never needed it — it is one
    // sequential loop — but the "Run now" button starts the same agents, and two
    // live passes would have the second one's `start` mark the first's row as
    // interrupted. Losing the race here simply means the work is already being
    // done.
    let target = system_agents::SystemAgents::target_of(agent, user_id);
    let Some(_claim) = skald.system_agents.claim(agent.id(), &target) else {
        info!(agent = agent.id(), user = %user_id,
              "system-agents: skipped — a run of this agent is already in progress");
        return;
    };

    let run_ctx = AgentRunCtx {
        user_id,
        pool:     &ctx.pool,
        sessions: &ctx.sessions,
        hub:      &ctx.chat_hub,
        subject:  None,
        run_id:   None,
    };

    // One user's failure must not end the pass for everyone after them.
    if let Err(e) = system_agents::run_and_record(agent, &run_ctx).await {
        warn!(agent = agent.id(), user = %user_id, error = %e, "system-agents: pass failed");
    }
}
