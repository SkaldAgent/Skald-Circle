use std::sync::Arc;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tracing::info;

pub use crate::db::tool_permission_groups::ToolPermissionGroup;
use crate::approval::{ApprovalManager, RuleAction};
use crate::tools::fs::{canonicalize_for_policy, path_under};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunContext {
    security_group:    Option<String>,
    #[serde(default)]
    pub system_prompt:     Vec<String>,
    #[serde(default)]
    pub allow_fs_writes:   Vec<String>,
    /// Extra directories/files granted read-only access (beyond the working directory,
    /// `docs/`, `skills/`, and everything in `allow_fs_writes`, which is readable too).
    #[serde(default)]
    pub allow_fs_reads:    Vec<String>,
    /// Project root (agent path `projects/{owner}/{slug}`) when this is a project
    /// session, `None` otherwise. The session working directory is always the user's
    /// home (`~`); the agent references project files via this absolute agent path,
    /// which `UserFs` routes to the per-member bind mount. Used to resolve
    /// `__PROJECT_ROOT__` placeholders in an agent's `inject_memory` paths.
    #[serde(default)]
    pub project_root:      Option<String>,
}

impl RunContext {
    pub fn with_security_group(security_group: Option<String>) -> Self {
        Self { security_group, ..Default::default() }
    }

    pub fn to_db(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    pub fn from_db(s: &str) -> Option<Self> {
        if s.is_empty() { return None; }
        serde_json::from_str(s).ok()
    }

    /// Permission group ID for approval rule lookup.
    pub fn tool_group_id(&self) -> Option<&str> {
        self.security_group.as_deref()
    }

    /// Combined system prompt fragments to inject as dynamic context, or None if empty.
    pub fn extra_system_prompt(&self) -> Option<String> {
        if self.system_prompt.is_empty() { return None; }
        Some(self.system_prompt.join("\n\n"))
    }

    /// True if writing to `path` is pre-authorized by this RunContext.
    /// Entries in `allow_fs_writes` are resolved against Skald's process cwd,
    /// so relative entries like `"data"` are treated as relative to the process cwd.
    /// Paths are canonicalized first (resolving `..`/symlinks), then matched as
    /// exact file OR recursive directory prefix.
    pub fn is_write_allowed(&self, path: &str) -> bool {
        if self.allow_fs_writes.is_empty() { return false; }
        let wd    = std::env::current_dir().unwrap_or_default();
        let canon = canonicalize_for_policy(path, &wd);
        self.allow_fs_writes.iter().any(|entry| {
            path_under(&canon, &canonicalize_for_policy(entry, &wd))
        })
    }

    /// True if reading `path` is pre-authorized by this RunContext.
    /// Read access is granted (no approval prompt) for: the process working directory
    /// itself, its `docs/` and `skills/` subtrees (always-safe baseline), any
    /// `allow_fs_reads` entry, and anything writable (write implies read). All paths
    /// are canonicalized first so `..`/symlink escapes cannot widen the grant.
    ///
    /// Note: this only relaxes a `Require` decision to `Allow` — an explicit `Deny`
    /// rule (e.g. on `secrets/`) still wins, because the approval engine is consulted
    /// first and `Deny` is never overridden by this fast-path.
    pub fn is_read_allowed(&self, path: &str) -> bool {
        let wd    = std::env::current_dir().unwrap_or_default();
        let canon = canonicalize_for_policy(path, &wd);

        let mut roots: Vec<std::path::PathBuf> = vec![
            canonicalize_for_policy(".",      &wd), // process working directory
            canonicalize_for_policy("docs",   &wd),
            canonicalize_for_policy("skills", &wd),
        ];
        roots.extend(self.allow_fs_reads.iter().map(|e| canonicalize_for_policy(e, &wd)));
        roots.extend(self.allow_fs_writes.iter().map(|e| canonicalize_for_policy(e, &wd)));

        roots.iter().any(|root| path_under(&canon, root))
    }
}

/// Outcome of validating a client-supplied [`RunContext`] against the caller's role.
pub enum RunContextDecision {
    /// Apply this (possibly sanitized) run-context to the session.
    Apply(Option<RunContext>),
    /// The requested security-group is not in the role's allowed set (→ 403); the
    /// string is the offending group id.
    Forbidden(String),
}

/// Gate a client-supplied run-context by the caller's role, closing two holes at
/// once (§0.1 — enforce server-side, never trust the client):
///
/// - **Group governance**: a non-admin may only select a security-group in its
///   role's effective set ([`crate::db::roles::role_allows_group`]); anything else
///   is [`RunContextDecision::Forbidden`].
/// - **fs escalation**: for a non-admin every other `RunContext` field
///   (`system_prompt`, `allow_fs_writes`/`allow_fs_reads`, `project_root`) is
///   **discarded** — the client can set the permission group, nothing more. A rich
///   run-context (a project's) is resolved server-side, never through this path.
///
/// `admin` is trusted and passes through unchanged. `None` (clear) is always
/// allowed and falls back to the role's default group at session build.
pub async fn validate_run_context_for_role(
    registry_pool: &SqlitePool,
    role_id:        &str,
    incoming:       Option<RunContext>,
) -> Result<RunContextDecision> {
    if role_id == crate::db::roles::ADMIN_ROLE_ID {
        return Ok(RunContextDecision::Apply(incoming));
    }
    let Some(rc) = incoming else {
        return Ok(RunContextDecision::Apply(None));
    };
    match rc.tool_group_id() {
        // A non-admin that names no group is treated as a clear (→ default group).
        None => Ok(RunContextDecision::Apply(None)),
        Some(group) => {
            if crate::db::roles::role_allows_group(registry_pool, role_id, group).await? {
                let group = group.to_string();
                Ok(RunContextDecision::Apply(Some(RunContext::with_security_group(Some(group)))))
            } else {
                Ok(RunContextDecision::Forbidden(group.to_string()))
            }
        }
    }
}

/// The catch-all permission group. A `RunContext` with no `security_group` resolves
/// here, and its rules are the fallback tier under *every* other group — so clearing
/// a group **widens** what a session may do, and is never the safe direction.
pub const DEFAULT_GROUP_ID: &str = "default";

/// The security group a session gets from its owner's role: `roles.permission_group`,
/// or `None` when that is unset or already the catch-all (nothing to pin).
pub async fn role_default_group(registry_pool: &SqlitePool, user_id: &str) -> Option<String> {
    let user  = crate::db::users::get(registry_pool, user_id).await.ok()??;
    let role  = crate::db::roles::get(registry_pool, &user.role_id).await.ok()??;
    let group = role.permission_group;
    (!group.is_empty() && group != DEFAULT_GROUP_ID).then_some(group)
}

/// A run-context for a **new** session carrying nothing but the role's default group,
/// so a restricted role starts scoped instead of on the catch-all.
pub async fn role_default_run_context(
    registry_pool: &SqlitePool,
    user_id:       &str,
) -> Option<RunContext> {
    role_default_group(registry_pool, user_id)
        .await
        .map(|g| RunContext::with_security_group(Some(g)))
}

/// Re-checks a **persisted** run-context's security group against the owner's
/// *current* role, degrading it to the role default when the role no longer allows it.
///
/// This is the counterpart of [`validate_run_context_for_role`], which gates a group
/// at *selection* time. The selected group is then persisted on `chat_sessions.
/// run_context` and was replayed verbatim on every later load — so revoking a group
/// from a role, or moving a user to a stricter role, left every session that already
/// had it running with it, indefinitely and across restarts. Running every load
/// through here makes the persisted value advisory rather than authoritative.
///
/// Deliberately **narrow**: only `security_group` is touched. A project session's
/// server-built context (`project_root`, `system_prompt`, fs grants) must survive
/// intact — unlike the selection path, which discards those because they came from
/// a client.
///
/// Conservative on uncertainty: with no group, an `admin` owner, or a role that
/// cannot be read, the context is returned unchanged. Guessing on a transient DB
/// error could only widen the session, which is the one outcome worth avoiding.
pub async fn reconcile_group_for_user(
    registry_pool: &SqlitePool,
    user_id:       &str,
    rc:            Option<RunContext>,
) -> Option<RunContext> {
    let mut rc = rc?;
    let Some(group) = rc.tool_group_id().map(str::to_string) else { return Some(rc) };

    let role_id = match crate::db::users::get(registry_pool, user_id).await {
        Ok(Some(u)) => u.role_id,
        other => {
            tracing::warn!(user = %user_id, group = %group, missing = other.is_ok(),
                "run_context: cannot resolve role, leaving the persisted security group in place");
            return Some(rc);
        }
    };
    if role_id == crate::db::roles::ADMIN_ROLE_ID {
        return Some(rc);
    }
    match crate::db::roles::role_allows_group(registry_pool, &role_id, &group).await {
        Ok(true) => return Some(rc),
        Ok(false) => {}
        Err(e) => {
            tracing::warn!(user = %user_id, group = %group, error = %e,
                "run_context: group check failed, leaving the persisted security group in place");
            return Some(rc);
        }
    }

    let replacement = role_default_group(registry_pool, user_id).await;
    info!(
        user = %user_id, role = %role_id, revoked = %group,
        now = replacement.as_deref().unwrap_or(DEFAULT_GROUP_ID),
        "run_context: security group no longer allowed by the role, degraded to the role default"
    );
    rc.security_group = replacement;
    Some(rc)
}

pub struct RunContextManager {
    db:       Arc<SqlitePool>,
    approval: Arc<ApprovalManager>,
}

impl RunContextManager {
    pub fn new(db: Arc<SqlitePool>, approval: Arc<ApprovalManager>) -> Self {
        Self { db, approval }
    }

    /// Seeds the built-in "default" permission group and migrates legacy rules.
    /// Safe to call at every startup (idempotent).
    pub async fn seed_defaults(&self) -> Result<()> {
        crate::db::tool_permission_groups::insert_or_ignore(
            &self.db, "default", "Default", Some("Built-in default permission group"),
        ).await?;

        let migrated = sqlx::query("UPDATE approval_rules SET group_id = 'default' WHERE group_id IS NULL")
            .execute(self.db.as_ref())
            .await
            .map(|r| r.rows_affected())
            .unwrap_or(0);

        if migrated > 0 {
            info!(%migrated, "run_context: migrated approval rules to 'default' group");
        }

        Ok(())
    }

    // ── ToolPermissionGroup CRUD ───────────────────────────────────────────────

    pub async fn list_groups(&self) -> Result<Vec<ToolPermissionGroup>> {
        crate::db::tool_permission_groups::list(&self.db).await
    }

    pub async fn get_group(&self, id: &str) -> Result<Option<ToolPermissionGroup>> {
        crate::db::tool_permission_groups::get(&self.db, id).await
    }

    pub async fn create_group(
        &self,
        id:          &str,
        name:        &str,
        description: Option<&str>,
    ) -> Result<()> {
        if id == "default" {
            bail!("cannot create a permission group with reserved id 'default'");
        }
        crate::db::tool_permission_groups::insert(&self.db, id, name, description).await
    }

    pub async fn update_group(
        &self,
        id:          &str,
        name:        &str,
        description: Option<&str>,
    ) -> Result<bool> {
        crate::db::tool_permission_groups::update(&self.db, id, name, description).await
    }

    pub async fn delete_group(&self, id: &str) -> Result<bool> {
        if id == "default" {
            bail!("cannot delete the built-in 'default' permission group");
        }
        crate::db::tool_permission_groups::delete(&self.db, id).await
    }

    /// Duplicates a permission group and all its rules atomically.
    pub async fn duplicate_group(
        &self,
        source_id: &str,
        new_id:    &str,
        new_name:  &str,
    ) -> Result<()> {
        if new_id == "default" {
            bail!("cannot create a permission group with reserved id 'default'");
        }
        let source = crate::db::tool_permission_groups::get(&self.db, source_id).await?
            .ok_or_else(|| anyhow::anyhow!("source group '{source_id}' not found"))?;

        let mut tx = self.db.begin().await?;

        sqlx::query(
            "INSERT INTO tool_permission_groups (id, name, description) VALUES (?, ?, ?)",
        )
        .bind(new_id)
        .bind(new_name)
        .bind(source.description.as_deref())
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO approval_rules \
                (agent_id, source, tool_pattern, path_pattern, action, note, priority, group_id) \
             SELECT agent_id, source, tool_pattern, path_pattern, action, note, priority, ? \
             FROM   approval_rules \
             WHERE  group_id = ?",
        )
        .bind(new_id)
        .bind(source_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    // ── Tool visibility ────────────────────────────────────────────────────────

    /// Returns the effective `RuleAction` for `tool_name` under the given permission group.
    /// `run_context_id` now directly holds a `tool_permission_groups` id (the run_contexts
    /// table indirection has been removed). Falls back to the `"default"` group when `None`.
    pub async fn check_tool_visibility(
        &self,
        run_context_id: Option<&str>,
        tool_name:      &str,
    ) -> Option<RuleAction> {
        let group_id = run_context_id.unwrap_or("default");
        self.approval.check_tool_visibility(group_id, tool_name).await
    }

    // ── Session assignment ─────────────────────────────────────────────────────

    /// Serialises `ctx` as JSON and stores it on the session row.
    /// `None` clears the context (falls back to the default permission group).
    pub async fn set_session_run_context(
        &self,
        session_id: i64,
        ctx:        Option<&RunContext>,
    ) -> Result<()> {
        let json = ctx.map(|rc| rc.to_db());
        sqlx::query("UPDATE chat_sessions SET run_context = ? WHERE id = ?")
            .bind(json.as_deref())
            .bind(session_id)
            .execute(self.db.as_ref())
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Creates a fresh, uniquely-named temp directory for an fs test.
    fn unique_tmp() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir()
            .join(format!("skald_rc_test_{}_{}", std::process::id(), nanos));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_allows_write_paths_and_extra_reads() {
        let writable = unique_tmp();
        let readable = unique_tmp();
        std::fs::write(writable.join("w.txt"), "x").unwrap();
        std::fs::write(readable.join("r.txt"), "x").unwrap();

        let rc = RunContext {
            allow_fs_writes:   vec![writable.to_string_lossy().into_owned()],
            allow_fs_reads:    vec![readable.to_string_lossy().into_owned()],
            ..Default::default()
        };
        // write implies read
        assert!(rc.is_read_allowed(writable.join("w.txt").to_str().unwrap()));
        assert!(rc.is_write_allowed(writable.join("w.txt").to_str().unwrap()));
        // read-only grant: readable but not writable
        assert!(rc.is_read_allowed(readable.join("r.txt").to_str().unwrap()));
        assert!(!rc.is_write_allowed(readable.join("r.txt").to_str().unwrap()));

        std::fs::remove_dir_all(&writable).ok();
        std::fs::remove_dir_all(&readable).ok();
    }

    #[test]
    fn canonicalize_resolves_parent_traversal() {
        let wd = unique_tmp();
        std::fs::create_dir_all(wd.join("docs")).unwrap();
        std::fs::create_dir_all(wd.join("secrets")).unwrap();
        std::fs::write(wd.join("secrets").join("s.txt"), "x").unwrap();

        assert_eq!(
            canonicalize_for_policy("docs/../secrets/s.txt", &wd),
            canonicalize_for_policy("secrets/s.txt", &wd),
        );

        std::fs::remove_dir_all(&wd).ok();
    }

    #[test]
    fn canonicalize_resolves_symlink_escape() {
        let wd = unique_tmp();
        std::fs::create_dir_all(wd.join("docs")).unwrap();
        std::fs::create_dir_all(wd.join("secrets")).unwrap();
        std::fs::write(wd.join("secrets").join("s.txt"), "x").unwrap();
        std::os::unix::fs::symlink(wd.join("secrets"), wd.join("docs").join("leak")).unwrap();

        // A symlink docs/leak -> secrets must resolve to the real secrets path.
        assert_eq!(
            canonicalize_for_policy("docs/leak/s.txt", &wd),
            canonicalize_for_policy("secrets/s.txt", &wd),
        );

        std::fs::remove_dir_all(&wd).ok();
    }

    #[test]
    fn write_allow_not_bypassed_by_traversal() {
        let wd = unique_tmp();
        std::fs::create_dir_all(wd.join("data")).unwrap();
        std::fs::create_dir_all(wd.join("secrets")).unwrap();

        let data_dir = wd.join("data").to_string_lossy().into_owned();
        let rc = RunContext {
            allow_fs_writes: vec![data_dir],
            ..Default::default()
        };
        // Writing into data/ is allowed...
        assert!(rc.is_write_allowed(wd.join("data").join("new.txt").to_str().unwrap()));
        // ...but data/../secrets/x escapes the grant and must NOT be allowed.
        assert!(!rc.is_write_allowed(wd.join("data").join("..").join("secrets").join("x.txt").to_str().unwrap()));

        std::fs::remove_dir_all(&wd).ok();
    }

    /// A registry with one role and one member of it. No crypto: the reconcile path
    /// only reads the directory, never a credential.
    async fn registry_with(role: &str, default_group: &str, extra_groups: &str) -> SqlitePool {
        let path = unique_tmp().join("system.db");
        let pool = crate::db::init_system_pool(path.to_str().unwrap()).await.unwrap();
        crate::db::roles::insert(&pool, role, "Role", default_group, Some(extra_groups))
            .await
            .unwrap();
        crate::db::users::insert(
            &pool, "u-1", "ada", None, role,
            &crate::db::users::Credentials::Cleartext(None),
        )
        .await
        .unwrap();
        pool
    }

    /// The regression: a group selected while the role allowed it was replayed from
    /// `chat_sessions.run_context` forever, so revoking it from the role changed
    /// nothing for sessions that already had it.
    #[tokio::test]
    async fn reconcile_degrades_a_group_the_role_no_longer_allows() {
        // The role's default is `kid`; `ops` is NOT in its set (it was, once).
        let pool = registry_with("member", "kid", r#"{"permission_groups":[]}"#).await;

        let rc = RunContext { security_group: Some("ops".into()), ..Default::default() };
        let got = reconcile_group_for_user(&pool, "u-1", Some(rc)).await.unwrap();
        assert_eq!(got.tool_group_id(), Some("kid"),
            "a revoked group must degrade to the role default, never to the catch-all");
    }

    /// Degrading must not silently widen: clearing to `None` would put a restricted
    /// user on the catch-all `default` group, whose rules are the fallback tier under
    /// every other group.
    #[tokio::test]
    async fn reconcile_keeps_an_allowed_group_and_preserves_the_rest_of_the_context() {
        let pool = registry_with("member", "kid", r#"{"permission_groups":["ops"]}"#).await;

        // Still allowed → untouched, including a project session's server-built fields,
        // which the *selection* path would have stripped.
        let rc = RunContext {
            security_group: Some("ops".into()),
            project_root:   Some("projects/ada/site".into()),
            system_prompt:  vec!["project brief".into()],
            ..Default::default()
        };
        let got = reconcile_group_for_user(&pool, "u-1", Some(rc)).await.unwrap();
        assert_eq!(got.tool_group_id(), Some("ops"));
        assert_eq!(got.project_root.as_deref(), Some("projects/ada/site"));
        assert_eq!(got.system_prompt, vec!["project brief".to_string()]);
    }

    /// A degrade must keep the rest of the context too — losing `project_root` would
    /// break a project chat as a side effect of a permissions edit.
    #[tokio::test]
    async fn reconcile_degrade_preserves_project_fields() {
        let pool = registry_with("member", "kid", r#"{"permission_groups":[]}"#).await;

        let rc = RunContext {
            security_group: Some("ops".into()),
            project_root:   Some("projects/ada/site".into()),
            ..Default::default()
        };
        let got = reconcile_group_for_user(&pool, "u-1", Some(rc)).await.unwrap();
        assert_eq!(got.tool_group_id(), Some("kid"));
        assert_eq!(got.project_root.as_deref(), Some("projects/ada/site"));
    }

    /// Uncertainty must never widen: an unknown user leaves the persisted group alone
    /// rather than falling back to the catch-all.
    #[tokio::test]
    async fn reconcile_leaves_the_group_alone_when_the_role_cannot_be_resolved() {
        let pool = registry_with("member", "kid", r#"{"permission_groups":[]}"#).await;

        let rc = RunContext { security_group: Some("ops".into()), ..Default::default() };
        let got = reconcile_group_for_user(&pool, "ghost", Some(rc)).await.unwrap();
        assert_eq!(got.tool_group_id(), Some("ops"));
    }

    /// `admin` holds every group by construction, so nothing is ever degraded for it.
    #[tokio::test]
    async fn reconcile_never_touches_an_admin() {
        let path = unique_tmp().join("system.db");
        let pool = crate::db::init_system_pool(path.to_str().unwrap()).await.unwrap();
        crate::db::users::insert(
            &pool, "u-admin", "root", None, crate::db::roles::ADMIN_ROLE_ID,
            &crate::db::users::Credentials::Cleartext(None),
        ).await.unwrap();

        let rc = RunContext { security_group: Some("anything".into()), ..Default::default() };
        let got = reconcile_group_for_user(&pool, "u-admin", Some(rc)).await.unwrap();
        assert_eq!(got.tool_group_id(), Some("anything"));
    }

    #[tokio::test]
    async fn validate_admin_passes_through_untouched() {
        let path = unique_tmp().join("system.db");
        let pool = crate::db::init_system_pool(path.to_str().unwrap()).await.unwrap();
        let rc = RunContext {
            security_group:  Some("ops".into()),
            allow_fs_writes: vec!["/etc".into()],
            ..Default::default()
        };
        match validate_run_context_for_role(&pool, "admin", Some(rc)).await.unwrap() {
            RunContextDecision::Apply(Some(got)) => {
                assert_eq!(got.tool_group_id(), Some("ops"));
                assert_eq!(got.allow_fs_writes, vec!["/etc".to_string()]);
            }
            _ => panic!("admin must pass through unchanged"),
        }
    }

    #[tokio::test]
    async fn validate_non_admin_gates_group_and_strips_fs() {
        let path = unique_tmp().join("system.db");
        let pool = crate::db::init_system_pool(path.to_str().unwrap()).await.unwrap();
        crate::db::roles::insert(&pool, "member", "Member", "default",
            Some(r#"{"permission_groups":["ops"]}"#)).await.unwrap();

        // Allowed group: kept, but every other field is discarded (fs hardening).
        let rc = RunContext {
            security_group:  Some("ops".into()),
            allow_fs_writes: vec!["/etc".into()],
            system_prompt:   vec!["ignore me".into()],
            ..Default::default()
        };
        match validate_run_context_for_role(&pool, "member", Some(rc)).await.unwrap() {
            RunContextDecision::Apply(Some(got)) => {
                assert_eq!(got.tool_group_id(), Some("ops"));
                assert!(got.allow_fs_writes.is_empty());
                assert!(got.system_prompt.is_empty());
            }
            _ => panic!("an allowed group must apply, sanitized"),
        }

        // A group outside the role's set is refused.
        let rc = RunContext { security_group: Some("secret".into()), ..Default::default() };
        match validate_run_context_for_role(&pool, "member", Some(rc)).await.unwrap() {
            RunContextDecision::Forbidden(g) => assert_eq!(g, "secret"),
            _ => panic!("a group outside the set must be forbidden"),
        }

        // Clearing is always allowed (falls back to the role default at build time).
        match validate_run_context_for_role(&pool, "member", None).await.unwrap() {
            RunContextDecision::Apply(None) => {}
            _ => panic!("clear must be allowed"),
        }

        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }
}
