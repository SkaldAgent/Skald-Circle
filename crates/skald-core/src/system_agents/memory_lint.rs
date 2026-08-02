//! The memory-lint agents — the weekly health pass over the two memory stores.
//!
//! Memory is a wiki, not a scrapbook (`agents/common/memory-wiki.md`), and a wiki
//! that nobody maintains rots: contradictions stay pending, dates go by, notes
//! lose their last inbound link, the same fact ends up written twice. The Lint
//! habit in the Schema covers "when you notice drift"; these agents are what
//! makes it happen when nobody notices.
//!
//! **There are two of them, and they are not the same job.** The private lint
//! runs for each user over their own store — their data, their notify, their run
//! log. The shared lint runs once over the group store, where the interesting
//! defect is different: a note that fails the table rule, i.e. one person's
//! private business sitting somewhere every member can read. They share the
//! wiki Schema through `agents/common/`, and diverge in their `AGENT.md`.
//!
//! **Both are read-only, and that is enforced twice.** The prompt says report,
//! never repair; and the approval rules already gate `shared-memory/*` writes as
//! `require` — so an agent that tried to fix something would raise an approval
//! card from an unattended pass, which [`super::run_ephemeral_turn`] auto-denies.
//! Read-only is therefore not a convention here, it is the only thing that works.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use sqlx::SqlitePool;

use core_api::{ConfigProperty, ConfigSet, PropertyType};

use crate::config_store::GlobalConfigManager;
use crate::db::memory_docs;
use crate::tools::fs::{SHARED_MEMORY_ROOT, USER_MEMORY_ROOT};

use super::{
    AgentOutcome, AgentRunCtx, AgentScope, SystemAgent, configured_run_context,
    enabled_from_config, enabled_property, interval_from_config, run_ephemeral_turn,
    security_group_property,
};

/// The chat `source` a lint pass runs under. Distinct from the user-facing
/// sources so a pass never lands in a conversation somebody is reading.
const LINT_SOURCE: &str = "memory-lint";

const DAY_SECS: u64 = 24 * 60 * 60;
/// A week, the default for both passes: long enough that a report is worth
/// reading, short enough that a contradiction does not sit for a month.
const DEFAULT_INTERVAL_SECS: u64 = 7 * DAY_SECS;

pub const PRIVATE_AGENT: &str = "memory-lint-private";
pub const SHARED_AGENT:  &str = "memory-lint-shared";

pub const PRIVATE_ENABLED_KEY:        &str = "memory_lint_private.enabled";
pub const PRIVATE_SECURITY_GROUP_KEY: &str = "memory_lint_private.security_group";
pub const PRIVATE_INTERVAL_DAYS_KEY:  &str = "memory_lint_private.interval_days";

pub const SHARED_ENABLED_KEY:        &str = "memory_lint_shared.enabled";
pub const SHARED_SECURITY_GROUP_KEY: &str = "memory_lint_shared.security_group";
pub const SHARED_INTERVAL_DAYS_KEY:  &str = "memory_lint_shared.interval_days";

/// The interval property, in **days**.
///
/// The unit is per-agent on purpose. Event triage is configured in minutes because it
/// runs in minutes; asking an admin to type `10080` for "weekly" would be a
/// worse form of the same field.
fn interval_days_property(key: &str, description: &str) -> ConfigProperty {
    ConfigProperty {
        key:           key.into(),
        name:          "Interval (days)".into(),
        description:   description.into(),
        property_type: PropertyType::Int,
        default_value: Some("7".into()),
    }
}

pub fn private_config_set() -> ConfigSet {
    ConfigSet {
        name:        "Private memory lint".into(),
        description: "A periodic health pass over each person's own memory store. For one user at \
                      a time it re-reads their notes and looks for drift: contradictions still \
                      pending, facts whose date has gone by, notes nothing links to, index lines \
                      pointing at nothing, and duplicates worth merging. It reports what it found \
                      as a notification and never edits anything itself. It reads only that \
                      user's private store, and the run is recorded on their own System agents \
                      page; a user who has not logged in since the last restart is skipped, \
                      because their database is still encrypted."
            .into(),
        properties:  vec![
            enabled_property(
                PRIVATE_ENABLED_KEY,
                "Enable the private memory lint for the whole instance. When disabled, nobody's \
                 private store is checked.",
            ),
            security_group_property(PRIVATE_SECURITY_GROUP_KEY),
            interval_days_property(
                PRIVATE_INTERVAL_DAYS_KEY,
                "How long between passes for each user. Counted per person from their own last \
                 pass, and it survives a restart, so a long interval is not reset by rebooting \
                 the machine.",
            ),
        ],
        owner:       Some(PRIVATE_AGENT.into()),
    }
}

pub fn shared_config_set() -> ConfigSet {
    ConfigSet {
        name:        "Shared memory lint".into(),
        description: "A periodic health pass over the group's shared memory. It looks for the \
                      same drift as the private pass, plus the defect that only exists here: a \
                      note that fails the table rule — one person's private business sitting \
                      where every member can read it. It reports and never edits. The shared \
                      store belongs to nobody, so the pass runs as the admin and its report goes \
                      to them; it needs an admin who has logged in since the last restart."
            .into(),
        properties:  vec![
            enabled_property(
                SHARED_ENABLED_KEY,
                "Enable the shared memory lint for the whole instance.",
            ),
            security_group_property(SHARED_SECURITY_GROUP_KEY),
            interval_days_property(
                SHARED_INTERVAL_DAYS_KEY,
                "How long between passes over the shared store. It survives a restart, so a long \
                 interval is not reset by rebooting the machine.",
            ),
        ],
        owner:       Some(SHARED_AGENT.into()),
    }
}

/// Shared by both agents: everything that differs is a field.
pub struct MemoryLintAgent {
    id:            &'static str,
    scope:         AgentScope,
    /// The store this pass reads: `user-memory` or `shared-memory`.
    root:          &'static str,
    enabled_key:   &'static str,
    group_key:     &'static str,
    interval_key:  &'static str,
    config_set:    fn() -> ConfigSet,
    config_store:  Arc<GlobalConfigManager>,
    /// `system.db` — the registry, read to reconcile the security group against
    /// the user's role, and (for the shared pass) the store itself.
    registry_pool: Arc<SqlitePool>,
}

impl MemoryLintAgent {
    /// The per-user pass over `user-memory/`.
    pub fn private(
        config_store:  Arc<GlobalConfigManager>,
        registry_pool: Arc<SqlitePool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id:           PRIVATE_AGENT,
            scope:        AgentScope::PerUser,
            root:         USER_MEMORY_ROOT,
            enabled_key:  PRIVATE_ENABLED_KEY,
            group_key:    PRIVATE_SECURITY_GROUP_KEY,
            interval_key: PRIVATE_INTERVAL_DAYS_KEY,
            config_set:   private_config_set,
            config_store,
            registry_pool,
        })
    }

    /// The instance pass over `shared-memory/`, run as the admin.
    pub fn shared(
        config_store:  Arc<GlobalConfigManager>,
        registry_pool: Arc<SqlitePool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id:           SHARED_AGENT,
            scope:        AgentScope::Instance,
            root:         SHARED_MEMORY_ROOT,
            enabled_key:  SHARED_ENABLED_KEY,
            group_key:    SHARED_SECURITY_GROUP_KEY,
            interval_key: SHARED_INTERVAL_DAYS_KEY,
            config_set:   shared_config_set,
            config_store,
            registry_pool,
        })
    }

    /// Which pool holds the store this agent lints: the caller's own for the
    /// private pass, `system.db` for the shared one (the same routing
    /// `classify_memory` gives the fs-tools).
    fn store_pool<'a>(&'a self, ctx: &'a AgentRunCtx<'_>) -> &'a SqlitePool {
        match self.scope {
            AgentScope::Instance => &self.registry_pool,
            // A lint is never per-subject; anything but the instance store is the
            // caller's own.
            _                    => ctx.pool,
        }
    }
}

#[async_trait]
impl SystemAgent for MemoryLintAgent {
    fn id(&self) -> &'static str { self.id }

    fn scope(&self) -> AgentScope { self.scope }

    fn config_set(&self) -> ConfigSet { (self.config_set)() }

    fn interval_key(&self) -> &'static str { self.interval_key }

    async fn is_enabled(&self) -> bool {
        enabled_from_config(&self.config_store, self.enabled_key).await
    }

    async fn interval_secs(&self) -> u64 {
        interval_from_config(
            &self.config_store,
            self.interval_key,
            DAY_SECS,
            DEFAULT_INTERVAL_SECS,
        )
        .await
    }

    /// Nothing to lint in an empty store. Worth checking: without it, a member
    /// who never uses memory would collect a weekly run row and a weekly
    /// notification saying there was nothing to report.
    async fn has_work(&self, ctx: &AgentRunCtx<'_>) -> Result<bool> {
        let notes = memory_docs::list(self.store_pool(ctx), "").await?;
        Ok(!notes.is_empty())
    }

    async fn run(&self, ctx: &AgentRunCtx<'_>) -> Result<AgentOutcome> {
        let notes = memory_docs::list(self.store_pool(ctx), "").await?;
        let rc = configured_run_context(
            &self.config_store,
            &self.registry_pool,
            self.group_key,
            ctx.user_id,
        )
        .await;

        let (session_id, notified) = run_ephemeral_turn(
            self.id,
            LINT_SOURCE,
            &build_prompt(self.root, notes.len()),
            rc.as_ref(),
            "Memory lint",
            std::collections::HashMap::new(),
            ctx,
        )
        .await?;

        Ok(AgentOutcome {
            session_id: Some(session_id),
            stats:      serde_json::json!({
                "notes_examined":        notes.len(),
                "notifications_emitted": notified,
            }),
        })
    }
}

/// The trigger message. Deliberately thin: *how* to lint is the agent's
/// `AGENT.md` plus the wiki Schema it includes from `agents/common/`, and
/// duplicating any of it here would give us two copies to keep in step.
fn build_prompt(root: &str, note_count: usize) -> String {
    let today = chrono::Utc::now().format("%Y-%m-%d");
    format!(
        "[LINT] Scheduled health pass over `{root}/` — {today}\n\
         The store currently holds {note_count} note(s).\n\n\
         Read the store, find what has drifted, and report it. Change nothing."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_names_the_store_and_forbids_editing() {
        let p = build_prompt(SHARED_MEMORY_ROOT, 12);
        assert!(p.contains("shared-memory/"));
        assert!(p.contains("12 note(s)"));
        assert!(p.contains("Change nothing."));
    }

    #[test]
    fn the_two_agents_do_not_share_config_keys() {
        let private: Vec<String> = private_config_set()
            .properties.into_iter().map(|p| p.key).collect();
        let shared: Vec<String> = shared_config_set()
            .properties.into_iter().map(|p| p.key).collect();

        // A shared key would make one agent's switch silently move the other's.
        for key in &private {
            assert!(!shared.contains(key), "`{key}` is claimed by both lint agents");
        }
    }
}
