//! Approval gate — centralised human-in-the-loop for tool execution.
//!
//! ## Architecture
//!
//! `ApprovalManager` is a top-level service (in `AppState`) shared by all
//! sessions. Its job is threefold:
//!
//! 1. **Rules** — decide, given (agent_id, session source, tool_name, args),
//!    whether a tool call needs human approval, is always allowed, or always
//!    denied. Rules live in `approval_rules` (SQLite) and are evaluated in
//!    priority order; the first match wins. If no rule matches the default is
//!    `Require` (default-closed). The `default` group additionally ships an
//!    explicit final `* require` catch-all (seeded only when absent) so the
//!    policy is visible/editable in the UI.
//!
//! 2. **Pending registry** — when a tool is gated, an in-memory entry tracks
//!    the waiting session. The web "Pending Approvals" page reads this list
//!    to show the human what needs deciding.
//!
//! 3. **Resolution** — `resolve(request_id, decision)` is called by the WS
//!    handler (or the Telegram approval bot in the future) to unblock the
//!    waiting session.
//!
//! ## Rule evaluation order
//!
//! Rules are sorted by `priority` ASC (lower = evaluated first). Within the
//! same priority, more-specific rules should be given a lower priority number.
//! The first rule whose `agent_id`, `source`, and `tool_pattern` all match
//! determines the action; if none matches the tool requires approval
//! (default-closed).
//!
//! ## Memory namespace
//!
//! The virtual memory roots `user-memory/` and `shared-memory/` (blueprint §5) are
//! allowed by **seeded rules**, not a hardcoded bypass: `seed_fs_path_rules` stamps
//! `@fs_any allow user-memory/*` and `@fs_any allow shared-memory/*`, editable in the
//! File System panel like any other path rule.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::sync::{broadcast, Mutex, oneshot};
use tracing::{debug, error, info, warn};

use crate::pending_registry::PendingRegistry;
use crate::session::handler::ApprovalDecision;
use crate::tools::tool_names as tn;
use crate::tools::ToolCategory;
use crate::events::{GlobalEvent, ServerEvent};

// ── Public types ──────────────────────────────────────────────────────────────

/// What the guardian concludes for a given tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateResult {
    /// No rule fired (or an explicit `allow` rule matched). Execute freely.
    Allow,
    /// An explicit `deny` rule matched. Reject immediately without asking.
    Deny,
    /// A `require` rule matched. Ask the human before executing.
    Require,
}

/// The action stored in an `approval_rules` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    /// Always require human approval.
    Require,
    /// Always allow (whitelist — skip the gate even if another rule would require it).
    Allow,
    /// Always deny (blacklist — tool call is rejected immediately).
    Deny,
}

impl RuleAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuleAction::Require => "require",
            RuleAction::Allow   => "allow",
            RuleAction::Deny    => "deny",
        }
    }
}

impl std::str::FromStr for RuleAction {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "require" => Ok(RuleAction::Require),
            "allow"   => Ok(RuleAction::Allow),
            "deny"    => Ok(RuleAction::Deny),
            other     => anyhow::bail!("unknown RuleAction: {other}"),
        }
    }
}

/// One row from `approval_rules`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRule {
    pub id:           i64,
    /// `None` matches any agent.
    pub agent_id:     Option<String>,
    /// `None` matches any source (`web`, `telegram`, `cron`, …).
    pub source:       Option<String>,
    /// Exact tool name or glob suffix: `mcp__gmail__*` matches all Gmail tools.
    pub tool_pattern: String,
    /// Optional glob on the normalised file path (e.g. `data/*`).
    /// Only meaningful for tools that carry a `path` argument.
    /// `None` = no path filter (matches any path, or tools without a path arg).
    /// `Some("data/*")` = only fires when `args["path"]` starts with `data/`.
    pub path_pattern: Option<String>,
    pub action:       RuleAction,
    pub note:         Option<String>,
    /// Lower priority number = evaluated first.
    pub priority:     i64,
    /// Permission group this rule belongs to. `None` is treated as `"default"`.
    pub group_id:     Option<String>,
}

/// Input for creating a new rule.
#[derive(Debug, Clone, Deserialize)]
pub struct NewApprovalRule {
    pub agent_id:     Option<String>,
    pub source:       Option<String>,
    pub tool_pattern: String,
    /// Optional glob on the normalised file path (e.g. `data/*`).
    pub path_pattern: Option<String>,
    pub action:       RuleAction,
    pub note:         Option<String>,
    pub priority:     Option<i64>,
    /// Permission group for this rule. Defaults to `"default"` when omitted.
    pub group_id:     Option<String>,
}

/// Public view of a pending approval request (no channel, safe to clone/serialize).
#[derive(Debug, Clone, Serialize)]
pub struct PendingApprovalInfo {
    pub request_id:    i64,
    pub session_id:    i64,
    pub tool_call_id:  i64,
    pub tool_name:     String,
    pub arguments:     Value,
    pub agent_id:      String,
    pub source:        String,
    /// Human-readable label for the origin (e.g. "CronJob: Daily Digest"). Null for web sessions.
    pub context_label: Option<String>,
    /// ISO-8601 timestamp string (UTC).
    pub created_at:    String,
    /// Registered tool category (None for MCP and unknown tools).
    pub tool_category: Option<ToolCategory>,
    /// MCP server name extracted from the tool name (e.g. "gmail" from "mcp__gmail__search").
    /// None for non-MCP tools.
    pub mcp_server:    Option<String>,
}

/// `request_id` value stamped on DB-rebuilt pending approvals (post-restart), where
/// there is no live registry entry / oneshot to address. It is falsy on purpose: the
/// frontend routes a falsy `request_id` to the durable `POST /api/tools/{tool_call_id}/resolve`
/// path instead of the in-memory WS/Inbox path (which would no-op with an empty registry).
/// Live approvals instead carry `request_id == tool_call_id`.
///
/// (Fully retiring this sentinel — so persisted items resolve uniformly by tool_call_id
/// everywhere, including mobile/telegram — is part of the deferred Approach B.)
pub const PERSISTED_REQUEST_ID: i64 = 0;

// ── Session bypass ────────────────────────────────────────────────────────────

/// What a session bypass entry applies to.
///
/// [`Tool`](Self::Tool) is the **default** scope of the "15 min" / "Session"
/// buttons on an approval card, and the only one narrow enough to be safe to
/// pick on the user's behalf: a human answering a card has read *that* call,
/// and nothing else. The wider scopes stay reachable through the REST
/// `bypass_scope` field, where choosing one is a deliberate act.
pub enum BypassScope {
    /// Covers every tool regardless of category.
    All,
    /// Covers exactly one tool, matched on its full name
    /// (`mcp__gmail__send_message`, `write_file`, …).
    Tool(String),
    /// Covers only tools of the given registered category.
    Category(ToolCategory),
    /// Covers only tools belonging to the named MCP server
    /// (matched by the `mcp__<server>__` prefix in the tool name).
    ///
    /// **A connector is not a permission unit**: its read tools and its write
    /// tools live under one name, so this scope reads "trust everything Gmail
    /// can do" — including sending mail — from a click on a card that asked
    /// about labelling a message. Never auto-detect it; require the caller to
    /// name it.
    McpServer(String),
}

/// A single in-memory bypass entry for a session.
///
/// Converts `GateResult::Require` → `Allow` for the matching scope.
/// `Deny` rules are never bypassed.
struct ApprovalBypass {
    scope:      BypassScope,
    /// `None` = no expiry (lasts until session ends / `clear_session_bypass` is called).
    expires_at: Option<Instant>,
}

// ── ApprovalManager ───────────────────────────────────────────────────────────

pub struct ApprovalManager {
    db:               Arc<SqlitePool>,
    /// Shared pending-request plumbing (map + oneshot). Keyed by the durable
    /// `tool_call_id` (= the `chat_llm_tools` rowid), which is also mirrored into
    /// `PendingApprovalInfo.request_id`. There is no separate in-memory id counter:
    /// `request_id == tool_call_id` for every live approval, so the "resolve by
    /// request_id" and "resolve by tool_call_id" paths are one and the same lookup.
    registry:         PendingRegistry<PendingApprovalInfo, ApprovalDecision>,
    session_bypasses: Mutex<HashMap<i64, Vec<ApprovalBypass>>>,
    event_tx:         broadcast::Sender<GlobalEvent>,
}

impl ApprovalManager {
    pub fn new(db: Arc<SqlitePool>, event_tx: broadcast::Sender<GlobalEvent>) -> Self {
        Self {
            db,
            registry:         PendingRegistry::new(),
            session_bypasses: Mutex::new(HashMap::new()),
            event_tx,
        }
    }

    // ── Seeding ───────────────────────────────────────────────────────────────

    /// Inserts the default rules (equivalent to the old hardcoded `needs_approval`)
    /// only if the table is currently empty. Safe to call at every startup.
    pub async fn seed_defaults(&self) -> Result<()> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM approval_rules")
            .fetch_one(self.db.as_ref())
            .await?;

        if count > 0 {
            return Ok(());
        }

        let defaults: &[(&str, &str)] = &[
            (tn::EXECUTE_CMD, "require"),
            // Opening a mobile pairing window emits a secret (the QR) into chat:
            // it must be a deliberate human action, not LLM-triggerable (plugin.md §11).
            ("mobile_start_pairing", "require"),
            // Binding/revoking a device assigns a phone to a user — a security
            // decision (who receives whose Inbox), so it must be human-gated too.
            ("mobile_bind_device", "require"),
            ("mobile_revoke_device", "require"),
        ];
        // NOTE: file-write tools are NOT seeded here as per-tool `require` rules.
        // Filesystem gating is owned by the "File System" category — path-scoped
        // `@fs_*` rules seeded in `seed_fs_path_rules`, backstopped by the `*` catch-all.

        for (pattern, action) in defaults {
            sqlx::query(
                "INSERT INTO approval_rules (tool_pattern, action, note, priority, group_id)
                 VALUES (?, ?, 'default rule', 10, 'default')",
            )
            .bind(pattern)
            .bind(action)
            .execute(self.db.as_ref())
            .await?;
        }

        // Whitelist benign built-in plumbing/UI tools so they don't prompt under the
        // require-by-default catch-all below. These carry no side effects worth gating.
        let benign_allows = &[
            tn::WRITE_TODOS,
            tn::NOTIFY,
            tn::ACTIVATE_TOOLS,
            tn::SHOW_FILE_TO_USER,
            "image_generate",
        ];
        for pattern in benign_allows {
            sqlx::query(
                "INSERT INTO approval_rules (tool_pattern, action, note, priority, group_id)
                 VALUES (?, 'allow', 'benign builtin', 0, 'default')",
            )
            .bind(pattern)
            .execute(self.db.as_ref())
            .await?;
        }

        // Final catch-all at the bottom of the default group: REQUIRE. Any tool not
        // matched by a more-specific rule falls here and prompts for approval. This is
        // the intended default-closed policy; whitelist/blacklist rules layer above it.
        sqlx::query(
            "INSERT INTO approval_rules (tool_pattern, action, note, priority, group_id)
             VALUES ('*', 'require', 'catch-all: require by default', 999999, 'default')",
        )
        .execute(self.db.as_ref())
        .await?;

        info!(
            "approval_rules seeded with {} default rules + {} benign allows + catch-all require",
            defaults.len(), benign_allows.len(),
        );
        Ok(())
    }

    /// Idempotent: ensures the Default group has a final catch-all `*` rule.
    ///
    /// Inserts `* require @999999` **only when no `*` catch-all exists** for the group.
    /// This is the default-closed fallback: any tool not matched by a more-specific rule
    /// prompts for approval. Because it only fires when the catch-all is *absent*, it
    /// never overwrites a catch-all the user changed from the frontend (allow/deny/require)
    /// and never re-creates the legacy allow-all. Runs at every startup (no-op when a
    /// `*` catch-all is already present).
    pub async fn seed_default_catch_all(&self) -> Result<()> {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM approval_rules
             WHERE tool_pattern = '*' AND group_id = 'default'",
        )
        .fetch_one(self.db.as_ref())
        .await?;

        if count > 0 {
            return Ok(());
        }

        sqlx::query(
            "INSERT INTO approval_rules (tool_pattern, action, note, priority, group_id)
             VALUES ('*', 'require', 'catch-all: require by default', 999999, 'default')",
        )
        .execute(self.db.as_ref())
        .await?;

        info!("approval_rules: seeded default catch-all require (priority 999999)");
        Ok(())
    }

    /// Seeds the default **File System** path rules using the `@fs_*` class tokens, so
    /// each default is a single, visible, editable row in the File System permission
    /// panel (one row per path, instead of one row per tool). Each entry is inserted
    /// independently only when its exact `(tool_pattern, path_pattern)` is absent, so a
    /// rule the user deleted is only ever re-created individually (matches the idempotency
    /// style of the other seeders). Runs at every startup.
    ///
    /// Priority `5` places these path-scoped rules *before* the `*` catch-all (999999),
    /// so an unmatched path still falls through to the default `require`.
    ///
    /// - `user-memory/*` → **allow** (the caller's private memory namespace, blueprint §5;
    ///   the LLM manages its own memory). Reads *and* writes: `@fs_any`.
    /// - `shared-memory/*` → reads **allow** (`@fs_read`), writes **require** (`@fs_write`):
    ///   shared memory is visible to everyone, so a write is a deliberate, human-confirmed
    ///   act — the agent must not silently push one person's information into it.
    /// - `shared-memory/log.md` + `append_file` → **allow**, at a lower priority number so it
    ///   is evaluated first: the audit trail must always be writable, and `append_file` is
    ///   the one write tool that cannot shorten a file.
    /// - `data/*` → **allow** (scratch/data workspace).
    /// - `skills/*` → reads **allow** (`@fs_read`): the trust decision on a skill is
    ///   taken at installation, not at each read. There is no write counterpart —
    ///   the whole tree is read-only in both directions (blueprint §9).
    /// - `memory_search` → **allow**, path-less: it searches note *content* (arg `query`,
    ///   not `path`), so it needs a tool-scoped rule rather than a path pattern.
    ///
    /// There is no `secrets/*` deny any more. It guarded an on-disk credential store
    /// that no longer exists — connectors now take their credentials from the
    /// activation form, held in the owner DB. Worse, it had quietly become wrong:
    /// fs-tool paths are rooted at the caller's own home (§6), so `secrets/*` had
    /// stopped meaning "the box's credential store" and started meaning "any folder a
    /// user dared name `secrets`".
    pub async fn seed_fs_path_rules(&self) -> Result<()> {
        // (tool_pattern, path_pattern, action, note, priority). `path_pattern = None`
        // is a tool-scoped rule that matches regardless of args. Priority 5 unless a
        // rule must be evaluated *before* a broader sibling — lower number wins.
        let rules: &[(&str, Option<&str>, &str, &str, i64)] = &[
            ("@fs_any",       Some("user-memory/*"),      "allow",   "auto-allow user-memory/", 5),
            // Ahead of the `shared-memory/*` write rule below: `log.md` is the
            // append-only audit trail of shared memory, and `append_file` is the one
            // write tool that cannot shorten a file. Gating it would be friction with
            // no safety — worse, a rejected log write yields an *unlogged* change,
            // which is exactly the failure the trail exists to prevent. Every other
            // shared write, and every other tool on `log.md`, still falls through to
            // `require`.
            ("append_file",   Some("shared-memory/log.md"), "allow", "auto-allow shared-memory audit log", 4),
            ("@fs_read",      Some("shared-memory/*"),    "allow",   "auto-allow read shared-memory/", 5),
            ("@fs_write",     Some("shared-memory/*"),    "require", "require write shared-memory/", 5),
            ("@fs_any",       Some("data/*"),             "allow",   "auto-allow data/", 5),
            // The skills tree (blueprint §7.2): reading a skill must never raise a
            // card. The trust decision was taken when it was *installed* — the
            // `skill_register` card — exactly as a connector is trusted at
            // activation and not at each call. Read-only is enforced by the mount
            // and by `UserFs::can_write_to`, so there is no write rule to pair
            // with this one; today `RunContext::is_read_allowed` would already
            // allow it, and this row is what keeps that true if the working
            // directory ever narrows (the binary-first direction).
            ("@fs_read",      Some("skills/*"),           "allow",   "auto-allow read skills/", 5),
            // Project folders (`projects/{owner}/{slug}`, blueprint §6): reads + writes
            // frictionless, matching the working-project UX. A read-only member's mount
            // is `:ro`, so a write physically fails regardless of this allow.
            ("@fs_any",       Some("projects/*"),         "allow",   "auto-allow projects/", 5),
            ("memory_search", None,                       "allow",   "allow memory_search", 5),
        ];

        let mut seeded = 0;
        for &(tool_pattern, path_pattern, action, note, priority) in rules {
            // A NULL path can't be matched with `=` (NULL comparisons are never true),
            // so the existence check branches on it — otherwise the row would re-insert
            // on every boot.
            let exists: i64 = match path_pattern {
                Some(p) => sqlx::query_scalar(
                    "SELECT COUNT(*) FROM approval_rules
                     WHERE tool_pattern = ? AND path_pattern = ? AND group_id = 'default'",
                )
                .bind(tool_pattern)
                .bind(p)
                .fetch_one(self.db.as_ref())
                .await?,
                None => sqlx::query_scalar(
                    "SELECT COUNT(*) FROM approval_rules
                     WHERE tool_pattern = ? AND path_pattern IS NULL AND group_id = 'default'",
                )
                .bind(tool_pattern)
                .fetch_one(self.db.as_ref())
                .await?,
            };
            if exists > 0 {
                continue;
            }
            sqlx::query(
                "INSERT INTO approval_rules (tool_pattern, path_pattern, action, note, priority, group_id)
                 VALUES (?, ?, ?, ?, ?, 'default')",
            )
            .bind(tool_pattern)
            .bind(path_pattern) // Option<&str> → NULL when None
            .bind(action)
            .bind(note)
            .bind(priority)
            .execute(self.db.as_ref())
            .await?;
            seeded += 1;
        }

        if seeded > 0 {
            info!("approval_rules: seeded {seeded} File System path rules (@fs_* tokens)");
        }
        Ok(())
    }

    /// One-time migration: removes the legacy per-tool filesystem rules that the new
    /// `@fs_*` File System panel supersedes, so they don't linger as un-editable advanced
    /// cards or shadow the new path rules. Identified by the exact `note` text the old
    /// seeders stamped (user-authored rules carry different notes and are untouched).
    ///
    /// Deletes:
    /// - the per-tool write `require` defaults (`note = 'default rule'`, no path) — fs
    ///   gating now lives in the File System panel + the `*` catch-all;
    /// - the old `data/*` allow rows (`note = 'auto-allow data/ writes'`);
    /// - both generations of `secrets/` deny row (`note = 'deny reading secrets/'` and
    ///   `'deny secrets/ access'`) — the on-disk secrets store is gone, and rooted at a
    ///   user's home the pattern had come to deny them any folder named `secrets`;
    /// - the old single `memory/*` allow row (`note = 'auto-allow memory/'`) — the memory
    ///   namespace split into `user-memory/` + `shared-memory/`, so the `memory/*` pattern
    ///   no longer routes and would otherwise linger as a stale allow on a disk `./memory/`.
    ///
    /// Idempotent: a no-op once the legacy rows are gone. Run before `seed_fs_path_rules`.
    pub async fn migrate_legacy_fs_rules(&self) -> Result<()> {
        let n1 = sqlx::query(
            "DELETE FROM approval_rules
             WHERE note = 'default rule' AND path_pattern IS NULL
               AND tool_pattern IN ('write_file', 'edit_file', 'insert_at_line', 'replace_lines')",
        )
        .execute(self.db.as_ref())
        .await?
        .rows_affected();

        let n2 = sqlx::query("DELETE FROM approval_rules WHERE note = 'auto-allow data/ writes'")
            .execute(self.db.as_ref())
            .await?
            .rows_affected();

        let n3 = sqlx::query(
            "DELETE FROM approval_rules
             WHERE note IN ('deny reading secrets/', 'deny secrets/ access')",
        )
        .execute(self.db.as_ref())
        .await?
        .rows_affected();

        let n4 = sqlx::query("DELETE FROM approval_rules WHERE note = 'auto-allow memory/'")
            .execute(self.db.as_ref())
            .await?
            .rows_affected();

        // An earlier build seeded `@fs_any allow shared-memory/*`; shared writes now
        // require approval, so that blanket allow must go (the new @fs_read/@fs_write
        // rows are seeded fresh by `seed_fs_path_rules`).
        let n5 = sqlx::query("DELETE FROM approval_rules WHERE note = 'auto-allow shared-memory/'")
            .execute(self.db.as_ref())
            .await?
            .rows_affected();

        let total = n1 + n2 + n3 + n4 + n5;
        if total > 0 {
            info!("approval_rules: migrated {total} legacy filesystem rules to @fs_* File System panel");
        }
        Ok(())
    }

    // ── Guardian ──────────────────────────────────────────────────────────────

    /// Main gate check: returns what the guardian has decided for this tool call.
    ///
    /// Evaluation order:
    /// 1. Rules for `group_id` first, then "default" group as fallback, sorted by priority ASC.
    ///    First match wins. (The `user-memory/` and `shared-memory/` auto-allows are seeded
    ///    `@fs_any allow …/*` rules, not a hardcoded exception — see `seed_fs_path_rules`.)
    /// 2. Session bypass: if a matching bypass is active, `Require` → `Allow`.
    ///    `Deny` is never bypassed.
    /// 3. No match → `Require` (default-closed policy).
    ///    The Default group ships an explicit final `* require @999999` catch-all (seeded
    ///    only when absent, editable from the frontend) that makes this policy visible;
    ///    other groups define their own catch-all (e.g. `readonly` uses `* deny`).
    pub async fn check(
        &self,
        session_id: i64,
        category:   Option<ToolCategory>,
        agent_id:   &str,
        source:     &str,
        tool_name:  &str,
        args:       &Value,
        group_id:   Option<&str>,
    ) -> GateResult {
        let rules = match crate::db::approval_rules::list_for_group(&self.db, group_id).await {
            Ok(r)  => r,
            Err(e) => {
                // Fail closed: this is a default-closed security gate (see module docs).
                // A transient DB error must NOT let an un-vetted tool call (execute_cmd,
                // restart, writes) through — require human approval instead.
                error!("approval: failed to load rules: {e} — defaulting to Require (fail-closed)");
                return GateResult::Require;
            }
        };

        let result = 'rules: {
            for rule in &rules {
                if !rule_matches(rule, agent_id, source, tool_name, args) {
                    continue;
                }
                debug!(
                    tool = tool_name, agent = agent_id, source,
                    rule_id = rule.id, action = ?rule.action,
                    rule_group = rule.group_id.as_deref().unwrap_or("default"),
                    "approval: rule matched"
                );
                break 'rules match rule.action {
                    RuleAction::Require => GateResult::Require,
                    RuleAction::Allow   => GateResult::Allow,
                    RuleAction::Deny    => GateResult::Deny,
                };
            }
            debug!(tool = tool_name, "approval: no rule matched → require");
            GateResult::Require
        };

        // Session bypass: converts Require → Allow when an active bypass matches.
        // Deny is intentionally not bypassable.
        if matches!(result, GateResult::Require) {
            let mut bypasses = self.session_bypasses.lock().await;
            if let Some(entries) = bypasses.get_mut(&session_id) {
                // Prune expired entries lazily.
                entries.retain(|b| b.expires_at.map_or(true, |t| Instant::now() < t));
                let active = entries.iter().any(|b| bypass_matches(b, category, tool_name));
                if active {
                    debug!(
                        tool = tool_name, session_id,
                        "approval: session bypass active → allow"
                    );
                    return GateResult::Allow;
                }
            }
        }

        result
    }

    // ── Pending registry ──────────────────────────────────────────────────────

    /// Registers a pending approval and returns `(request_id, rx)`.
    /// The caller should send `ApprovalRequired` / `PendingWrite` events to
    /// the frontend and then `await rx` to block until the human responds.
    pub async fn register(
        &self,
        session_id:    i64,
        tool_call_id:  i64,
        tool_name:     &str,
        arguments:     Value,
        agent_id:      &str,
        source:        &str,
        context_label: Option<&str>,
        category:      Option<ToolCategory>,
    ) -> (i64, oneshot::Receiver<ApprovalDecision>) {
        // The durable tool_call_id IS the identity: no separate in-memory counter.
        // `request_id == tool_call_id` keeps the wire (events / JS / plugins) unchanged
        // while making the id stable and DB-derivable.
        let request_id = tool_call_id;

        let info = PendingApprovalInfo {
            request_id,
            session_id,
            tool_call_id,
            tool_name:     tool_name.to_string(),
            arguments,
            agent_id:      agent_id.to_string(),
            source:        source.to_string(),
            context_label: context_label.map(str::to_string),
            created_at:    chrono::Utc::now().to_rfc3339(),
            tool_category: category,
            mcp_server:    mcp_server_from_tool_name(tool_name),
        };

        let rx = self.registry.insert(request_id, info).await;
        info!(
            session_id, tool = tool_name, agent = agent_id, source, request_id,
            "approval: pending registered"
        );
        // Broadcast on the global bus so Inbox subscribers (e.g. the
        // mobile-connector plugin) can re-snapshot. This is the bus counterpart
        // of the per-session `ApprovalRequired` WS event.
        let _ = self.event_tx.send(GlobalEvent {
            source:     Some(source.to_string()),
            session_id: Some(session_id),
            event:      ServerEvent::ApprovalRequested {
                request_id,
                tool_call_id,
                tool_name: tool_name.to_string(),
            },
        });
        (request_id, rx)
    }

    /// Resolves a pending approval, unblocks the waiting session, and broadcasts
    /// `ApprovalResolved` to all WebSocket subscribers.
    pub async fn resolve(&self, request_id: i64, decision: ApprovalDecision) -> Option<PendingApprovalInfo> {
        let approved = matches!(decision, ApprovalDecision::Approved);
        match self.registry.resolve(request_id, decision).await {
            Some(info) => {
                let verb = if approved { "approved" } else { "rejected" };
                info!(
                    request_id, tool = info.tool_name,
                    session_id = info.session_id, verb,
                    "approval: resolved"
                );
                let _ = self.event_tx.send(GlobalEvent {
                    source:     Some(info.source.clone()),
                    session_id: Some(info.session_id),
                    event:      ServerEvent::ApprovalResolved {
                        request_id,
                        tool_call_id: info.tool_call_id,
                        approved,
                    },
                });
                Some(info)
            }
            None => {
                warn!(request_id, "approval: resolve called for unknown/already-resolved request_id");
                None
            }
        }
    }

    /// Convenience wrapper: approve a pending request.
    pub async fn approve(&self, request_id: i64) -> Option<PendingApprovalInfo> {
        self.resolve(request_id, ApprovalDecision::Approved).await
    }

    /// Convenience wrapper: reject a pending request with an optional note.
    pub async fn reject(&self, request_id: i64, note: String) -> Option<PendingApprovalInfo> {
        self.resolve(request_id, ApprovalDecision::Rejected { note }).await
    }

    /// Resolves a pending approval by `tool_call_id` (used by the REST endpoint,
    /// which knows the DB tool id but not the in-memory `request_id`).
    /// Returns `true` if an active pending entry was found and resolved,
    /// `false` if no matching entry exists (e.g. the app was restarted).
    /// Resolve a pending approval by `tool_call_id`. Since `request_id == tool_call_id`,
    /// this is the same lookup as [`resolve`] — kept as a named entry point for the REST
    /// `/api/tools/{tool_call_id}/resolve` endpoint, which knows the DB id but not the
    /// (now identical) request_id. Returns `true` if a live entry was found and resolved.
    pub async fn resolve_for_tool_call(
        &self,
        tool_call_id: i64,
        decision:     ApprovalDecision,
    ) -> bool {
        self.resolve(tool_call_id, decision).await.is_some()
    }

    /// Returns the in-memory `request_id` for a pending approval identified by its
    /// `tool_call_id`, if a live entry exists. Lets a history-rebuilt tool card be
    /// resolved through the source-agnostic WS/Inbox path (which keys on
    /// `request_id`, with bypass support) while the server is still up; returns
    /// `None` after a restart, when the caller falls back to resolving by the
    /// durable `tool_call_id`.
    pub async fn request_id_for_tool_call(&self, tool_call_id: i64) -> Option<i64> {
        // request_id == tool_call_id: return it iff a live entry exists (so a
        // history-rebuilt card resolves via the WS/Inbox path while the server is up),
        // else None (post-restart, caller falls back to resolving by tool_call_id).
        self.registry.get(tool_call_id).await.map(|_| tool_call_id)
    }

    /// Drops all pending entries for a session (called when WS disconnects).
    /// The waiting `await rx` in `llm_loop` will get `Err(RecvError)` and
    /// return `TurnOutcome::Cancelled`.
    pub async fn cancel_for_session(&self, session_id: i64) {
        let dropped = self.registry.remove_where(|i| i.session_id == session_id).await;
        if dropped > 0 {
            info!(session_id, dropped, "approval: cancelled pending entries (WS disconnected)");
        }
        self.session_bypasses.lock().await.remove(&session_id);
    }

    // ── Session bypass ────────────────────────────────────────────────────────

    /// Returns a snapshot of a single pending approval by `request_id`, without resolving it.
    pub async fn get_pending(&self, request_id: i64) -> Option<PendingApprovalInfo> {
        self.registry.get(request_id).await
    }

    /// Bypasses all approval prompts for `session_id` for the rest of the session.
    pub async fn bypass_session(&self, session_id: i64) {
        self.session_bypasses.lock().await
            .entry(session_id)
            .or_default()
            .push(ApprovalBypass { scope: BypassScope::All, expires_at: None });
        info!(session_id, "approval: bypass active (session)");
    }

    /// Bypasses all approval prompts for `session_id` for `duration`.
    pub async fn bypass_session_for(&self, session_id: i64, duration: Duration) {
        self.session_bypasses.lock().await
            .entry(session_id)
            .or_default()
            .push(ApprovalBypass { scope: BypassScope::All, expires_at: Some(Instant::now() + duration) });
        info!(session_id, secs = duration.as_secs(), "approval: bypass active (timed)");
    }

    /// Bypasses approval prompts for one tool, matched on its full name.
    /// `duration` is `None` for an indefinite (session-scoped) bypass.
    pub async fn bypass_session_for_tool(
        &self,
        session_id: i64,
        tool:       String,
        duration:   Option<Duration>,
    ) {
        let expires_at = duration.map(|d| Instant::now() + d);
        self.session_bypasses.lock().await
            .entry(session_id)
            .or_default()
            .push(ApprovalBypass { scope: BypassScope::Tool(tool.clone()), expires_at });
        info!(session_id, tool, secs = duration.map(|d| d.as_secs()), "approval: bypass active (tool)");
    }

    /// Bypasses approval prompts for a specific tool `category`.
    /// `duration` is `None` for an indefinite (session-scoped) bypass.
    pub async fn bypass_session_for_category(
        &self,
        session_id: i64,
        category:   ToolCategory,
        duration:   Option<Duration>,
    ) {
        let expires_at = duration.map(|d| Instant::now() + d);
        self.session_bypasses.lock().await
            .entry(session_id)
            .or_default()
            .push(ApprovalBypass { scope: BypassScope::Category(category), expires_at });
        info!(session_id, ?category, secs = duration.map(|d| d.as_secs()), "approval: bypass active (category)");
    }

    /// Bypasses approval prompts for all tools belonging to `mcp_server`.
    /// `duration` is `None` for an indefinite (session-scoped) bypass.
    pub async fn bypass_session_for_mcp(
        &self,
        session_id: i64,
        mcp_server: String,
        duration:   Option<Duration>,
    ) {
        let expires_at = duration.map(|d| Instant::now() + d);
        self.session_bypasses.lock().await
            .entry(session_id)
            .or_default()
            .push(ApprovalBypass { scope: BypassScope::McpServer(mcp_server.clone()), expires_at });
        info!(session_id, mcp_server, secs = duration.map(|d| d.as_secs()), "approval: bypass active (mcp_server)");
    }

    /// Removes all bypass entries for a session.
    pub async fn clear_session_bypass(&self, session_id: i64) {
        self.session_bypasses.lock().await.remove(&session_id);
        info!(session_id, "approval: bypass cleared");
    }

    /// Returns a snapshot of all currently-pending approvals (for the web page).
    pub async fn list_pending(&self) -> Vec<PendingApprovalInfo> {
        self.registry.list().await
    }

    /// Reconstructs pending approvals from the DB (`chat_llm_tools.status='pending'`,
    /// excluding `ask_user_clarification`, which shares that status). Used to keep the
    /// Inbox populated after a server restart, when the in-memory registry is empty.
    /// `request_id` is [`PERSISTED_REQUEST_ID`] — a falsy sentinel telling the client to
    /// resolve by `tool_call_id` (there is no live registry entry / oneshot to address).
    pub async fn list_persisted_pending(&self) -> Vec<PendingApprovalInfo> {
        let rows = sqlx::query_as::<_, (i64, String, Option<String>, String, i64, String, String)>(
            "SELECT t.id, t.name, t.arguments, t.created_at, ss.session_id, ss.agent_id, cs.source
             FROM   chat_llm_tools t
             JOIN   chat_history h          ON h.id  = t.message_id
             JOIN   chat_sessions_stack ss  ON ss.id = h.session_stack_id
             JOIN   chat_sessions cs        ON cs.id = ss.session_id
             WHERE  t.status = 'pending' AND t.name != 'ask_user_clarification'",
        )
        .fetch_all(&*self.db)
        .await
        .unwrap_or_default();

        rows.into_iter().map(|(id, name, args, created_at, session_id, agent_id, source)| {
            let arguments = args.as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(Value::Null);
            PendingApprovalInfo {
                request_id:    PERSISTED_REQUEST_ID,
                session_id,
                tool_call_id:  id,
                tool_name:     name.clone(),
                arguments,
                agent_id,
                source,
                context_label: None,
                created_at,
                tool_category: None,
                mcp_server:    mcp_server_from_tool_name(&name),
            }
        }).collect()
    }

    // ── Tool visibility ───────────────────────────────────────────────────────

    /// Returns `false` only when the first matching rule (by tool_pattern) is `Deny`.
    /// Path/agent/source filters are intentionally ignored — this is a static
    /// "is the tool offered to the LLM at all?" check, not an execution-time gate.
    /// Rules must already be loaded via `list_for_group`.
    pub fn is_tool_visible(&self, rules: &[ApprovalRule], tool_name: &str) -> bool {
        for rule in rules {
            if pattern_matches(&rule.tool_pattern, tool_name) {
                return rule.action != RuleAction::Deny;
            }
        }
        true // no matching rule → visible (backward compatible)
    }

    /// Resolves the effective `RuleAction` for `tool_name` in `group_id`,
    /// evaluating only `tool_pattern` (no path/agent/source).
    /// Returns `None` when no rule matches (tool is implicitly visible).
    pub async fn check_tool_visibility(
        &self,
        group_id:  &str,
        tool_name: &str,
    ) -> Option<RuleAction> {
        let rules = crate::db::approval_rules::list_for_group(&self.db, Some(group_id))
            .await
            .unwrap_or_default();
        for rule in &rules {
            if pattern_matches(&rule.tool_pattern, tool_name) {
                return Some(rule.action.clone());
            }
        }
        None
    }

    // ── Rule management ───────────────────────────────────────────────────────

    pub async fn list_rules(&self) -> Result<Vec<ApprovalRule>> {
        crate::db::approval_rules::list(&self.db).await
    }

    pub async fn add_rule(&self, r: NewApprovalRule) -> Result<i64> {
        if r.tool_pattern == "*" {
            let group = r.group_id.as_deref().unwrap_or("default");
            self.ensure_no_conflicting_catch_all(group, &r.action, None).await?;
        }
        crate::db::approval_rules::insert(&self.db, r).await
    }

    pub async fn delete_rule(&self, id: i64) -> Result<()> {
        crate::db::approval_rules::delete(&self.db, id).await
    }

    pub async fn update_rule(&self, id: i64, r: NewApprovalRule) -> Result<()> {
        if r.tool_pattern == "*" {
            let group = r.group_id.as_deref().unwrap_or("default");
            self.ensure_no_conflicting_catch_all(group, &r.action, Some(id)).await?;
        }
        crate::db::approval_rules::update(&self.db, id, r).await
    }

    /// Rejects creating/updating a `*` catch-all when the target group already has a
    /// **different** `*` catch-all. Two conflicting catch-alls shadow each other silently
    /// (lower priority number wins regardless of action) — the exact class of bug that let
    /// an `allow *` mask a `require *`. Editing the *same* row (matched by `exclude_id`) is
    /// always allowed, so the general rule stays changeable from the frontend.
    async fn ensure_no_conflicting_catch_all(
        &self,
        group_id:   &str,
        action:     &RuleAction,
        exclude_id: Option<i64>,
    ) -> Result<()> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT id, action FROM approval_rules
             WHERE tool_pattern = '*' AND group_id = ?",
        )
        .bind(group_id)
        .fetch_all(self.db.as_ref())
        .await?;

        for (id, existing_action) in rows {
            if Some(id) == exclude_id {
                continue;
            }
            let existing: RuleAction = existing_action.parse()?;
            if &existing != action {
                anyhow::bail!(
                    "group '{group_id}' already has a '*' catch-all with action '{}' \
                     (rule id {id}); conflicting catch-alls shadow silently — edit or \
                     remove it first",
                    existing.as_str(),
                );
            }
        }
        Ok(())
    }

    /// Approve + register a session bypass so future calls of the **same tool**
    /// are auto-approved.
    ///
    /// - `bypass_secs = Some(n)`: bypass lasts `n` seconds (0 is treated as indefinite)
    /// - `bypass_secs = None`: bypass lasts until the session ends
    ///
    /// The scope is always [`BypassScope::Tool`] and is deliberately **not**
    /// inferred from the tool's category or MCP server. It used to be: a click
    /// on a Gmail card registered a bypass over the whole connector, so
    /// approving `mcp__gmail__modify_message` silently un-gated
    /// `mcp__gmail__send_message` — an explicit `require` rule on it and all —
    /// and the only trace was a log line. A human answering a card has read one
    /// call; that call is the widest thing their click may authorise. The
    /// broader scopes remain available to a caller that names one (the REST
    /// `bypass_scope` field in `src/frontend/api/inbox.rs`).
    pub async fn approve_with_bypass(&self, request_id: i64, bypass_secs: Option<u64>) {
        let info = self.get_pending(request_id).await;
        self.approve(request_id).await;
        let Some(info) = info else { return };
        let duration = bypass_secs
            .filter(|&s| s > 0)
            .map(Duration::from_secs);
        self.bypass_session_for_tool(info.session_id, info.tool_name, duration).await;
    }
}

// ── ApprovalApi trait impl ────────────────────────────────────────────────────

#[async_trait::async_trait]
impl core_api::approval::ApprovalApi for ApprovalManager {
    async fn approve(&self, request_id: i64) {
        self.approve(request_id).await;
    }

    async fn reject(&self, request_id: i64, note: String) {
        self.reject(request_id, note).await;
    }

    async fn approve_with_bypass(&self, request_id: i64, bypass_secs: Option<u64>) {
        self.approve_with_bypass(request_id, bypass_secs).await;
    }
}

// ── Matching helpers ──────────────────────────────────────────────────────────

/// Returns `true` if the rule applies to the given (agent_id, source, tool_name, args).
fn rule_matches(rule: &ApprovalRule, agent_id: &str, source: &str, tool_name: &str, args: &Value) -> bool {
    // agent_id filter
    if let Some(ref ra) = rule.agent_id {
        if ra != agent_id {
            return false;
        }
    }
    // source filter
    if let Some(ref rs) = rule.source {
        if rs != source {
            return false;
        }
    }
    // tool pattern (exact, `prefix*` glob, or `@fs_*` filesystem class token)
    if !tool_pattern_matches(&rule.tool_pattern, tool_name) {
        return false;
    }
    // path_pattern filter: if set, args["path"] must match
    if let Some(ref pp) = rule.path_pattern {
        let path = args["path"].as_str().unwrap_or("");
        let norm = normalize_path(path);
        if !path_pattern_matches(pp, &norm) {
            return false;
        }
    }
    true
}

/// Matches a rule's `tool_pattern` against a concrete tool name.
///
/// In addition to the plain [`pattern_matches`] syntax (`*` / `prefix*` / exact), three
/// synthetic **filesystem class tokens** are recognised so a single rule can target a
/// whole operation class regardless of the individual tool name:
///
/// - `@fs_read`  — any file-read tool (`read_file`, `grep_files`, …)
/// - `@fs_write` — any file-write tool (`write_file`, `edit_file`, …)
/// - `@fs_any`   — any filesystem tool (read *or* write)
///
/// These back the "File System" permission panel, where each path row is one rule row.
pub(crate) fn tool_pattern_matches(pattern: &str, tool_name: &str) -> bool {
    match pattern {
        "@fs_read"  => crate::tools::is_file_read_tool(tool_name),
        "@fs_write" => crate::tools::is_file_write_tool(tool_name),
        "@fs_any"   => {
            crate::tools::is_file_read_tool(tool_name)
                || crate::tools::is_file_write_tool(tool_name)
        }
        _ => pattern_matches(pattern, tool_name),
    }
}

/// Matches a rule's `path_pattern` against a normalised (project-relative) path.
///
/// A `"<dir>/*"` pattern is treated as a **directory subtree**: it matches the directory
/// node itself (`path == "<dir>"`) *and* everything under it (`path` starts with
/// `"<dir>/"`). Unlike the raw trailing-`*` string match in [`pattern_matches`], this uses
/// a `/`-delimited boundary, so `memory/*` matches `memory` and `memory/x` but not the
/// sibling `memory-secrets/x`. All other patterns fall back to [`pattern_matches`]
/// (exact / trailing-`*`), preserving legacy `data/*`-style behaviour.
pub(crate) fn path_pattern_matches(pattern: &str, path: &str) -> bool {
    if let Some(dir) = pattern.strip_suffix("/*") {
        return path == dir || path.starts_with(&format!("{dir}/"));
    }
    pattern_matches(pattern, path)
}

/// Matches an exact name or a `prefix*` glob.
pub(crate) fn pattern_matches(pattern: &str, tool_name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        tool_name.starts_with(prefix)
    } else {
        pattern == tool_name
    }
}

/// Returns `true` when an `ApprovalBypass` entry covers the given tool.
fn bypass_matches(bypass: &ApprovalBypass, category: Option<ToolCategory>, tool_name: &str) -> bool {
    match &bypass.scope {
        BypassScope::All              => true,
        BypassScope::Tool(name)       => name == tool_name,
        BypassScope::Category(bc)     => category.map_or(false, |tc| tc == *bc),
        BypassScope::McpServer(server) => {
            mcp_server_from_tool_name(tool_name).map_or(false, |s| s == *server)
        }
    }
}

/// Extracts the MCP server name from a tool name following the `mcp__<server>__<tool>` pattern.
fn mcp_server_from_tool_name(name: &str) -> Option<String> {
    let rest = name.strip_prefix("mcp__")?;
    let end  = rest.find("__")?;
    Some(rest[..end].to_string())
}

/// Normalises a file path to a project-relative form for rule matching.
///
/// The path is canonicalized first (resolving `..` and symlinks via
/// `canonicalize_for_policy`) so that `docs/../secrets/x` or a symlink into `secrets/`
/// cannot evade a path-pattern rule. If the canonical path falls under the process
/// working directory it is made relative to it; otherwise the leading `/` is stripped
/// as a best-effort fallback.
pub(crate) fn normalize_path(path: &str) -> String {
    let cwd = std::env::current_dir().unwrap_or_default();
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let canon = crate::tools::fs::canonicalize_for_policy(path, &cwd);
    if let Ok(rel) = canon.strip_prefix(&cwd) {
        return rel.to_string_lossy().into_owned();
    }
    canon.to_string_lossy().trim_start_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::{path_pattern_matches, pattern_matches, tool_pattern_matches};

    #[test]
    fn fs_class_tokens_resolve_by_tool_class() {
        // @fs_read — read tools only
        assert!(tool_pattern_matches("@fs_read", "read_file"));
        assert!(tool_pattern_matches("@fs_read", "grep_files"));
        assert!(!tool_pattern_matches("@fs_read", "write_file"));

        // @fs_write — write tools only
        assert!(tool_pattern_matches("@fs_write", "write_file"));
        assert!(tool_pattern_matches("@fs_write", "insert_at_line"));
        assert!(!tool_pattern_matches("@fs_write", "read_file"));

        // @fs_any — any filesystem tool (read or write)
        assert!(tool_pattern_matches("@fs_any", "read_file"));
        assert!(tool_pattern_matches("@fs_any", "write_file"));

        // Non-filesystem tools never match the fs class tokens.
        for token in ["@fs_read", "@fs_write", "@fs_any"] {
            assert!(!tool_pattern_matches(token, "execute_cmd"));
            assert!(!tool_pattern_matches(token, "notify"));
        }
    }

    #[test]
    fn fs_tokens_fall_back_to_plain_pattern_for_non_tokens() {
        assert!(tool_pattern_matches("write_file", "write_file"));
        assert!(tool_pattern_matches("*", "anything"));
        assert!(tool_pattern_matches("mcp__gmail__*", "mcp__gmail__search"));
        assert!(!tool_pattern_matches("write_file", "read_file"));
    }

    #[test]
    fn dir_subtree_pattern_matches_dir_node_and_children_only() {
        // `<dir>/*` matches the directory node itself and everything under it…
        assert!(path_pattern_matches("memory/*", "memory"));
        assert!(path_pattern_matches("memory/*", "memory/notes.md"));
        assert!(path_pattern_matches("memory/*", "memory/sub/deep.md"));
        // …but NOT a sibling that merely shares the string prefix.
        assert!(!path_pattern_matches("memory/*", "memory-secrets/leak.md"));
        assert!(!path_pattern_matches("memory/*", "memoryx"));
        assert!(!path_pattern_matches("memory/*", "other/memory"));
    }

    #[test]
    fn non_subtree_path_patterns_fall_back_to_plain_match() {
        // Exact file path
        assert!(path_pattern_matches("config.yml", "config.yml"));
        assert!(!path_pattern_matches("config.yml", "config.yml.bak"));
        // Legacy trailing-`*` prefix semantics preserved
        assert!(path_pattern_matches("data*", "data"));
        assert!(path_pattern_matches("data*", "database/x"));
        // Sanity: the raw primitive still behaves as before
        assert!(pattern_matches("data/*", "data/x"));
    }

    /// A bypass answered from a card covers **that tool only**.
    ///
    /// The regression: approving `mcp__gmail__modify_message` with "15 min" used to
    /// register a bypass over the whole `gmail` connector, so the very next
    /// `mcp__gmail__send_message` executed without a prompt — through an explicit
    /// `require` rule written for it — and the only evidence was a log line.
    #[tokio::test]
    async fn a_tool_bypass_does_not_cover_its_connector() {
        use super::{ApprovalManager, GateResult};
        use serde_json::json;
        use std::sync::Arc;
        use tokio::sync::broadcast;

        let path = std::env::temp_dir().join(format!("skald_bypass_test_{}.db", std::process::id()));
        let path_str = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);
        let pool = crate::db::init_system_pool(&path_str).await.expect("init_system_pool");
        let db = Arc::new(pool);

        sqlx::query("INSERT INTO tool_permission_groups (id, name) VALUES ('default', 'Default')")
            .execute(db.as_ref()).await.unwrap();
        for (tool, action) in [
            ("mcp__gmail__modify_message", "require"),
            ("mcp__gmail__send_message",   "require"),
        ] {
            sqlx::query(
                "INSERT INTO approval_rules (tool_pattern, action, priority, group_id)
                 VALUES (?, ?, 0, 'default')",
            )
            .bind(tool).bind(action).execute(db.as_ref()).await.unwrap();
        }

        let (tx, _rx) = broadcast::channel(16);
        let mgr = ApprovalManager::new(Arc::clone(&db), tx);
        mgr.seed_default_catch_all().await.unwrap();

        let decide = |tool: &'static str| {
            let mgr = &mgr;
            async move {
                mgr.check(1, None, "assistant", "web", tool, &json!({}), Some("default")).await
            }
        };

        // Both gated to begin with.
        assert!(matches!(decide("mcp__gmail__modify_message").await, GateResult::Require));
        assert!(matches!(decide("mcp__gmail__send_message").await,   GateResult::Require));

        // The human approves ONE call with a bypass.
        mgr.bypass_session_for_tool(1, "mcp__gmail__modify_message".into(), None).await;

        // It covers that tool…
        assert!(matches!(decide("mcp__gmail__modify_message").await, GateResult::Allow));
        // …and nothing else on the same connector.
        assert!(matches!(decide("mcp__gmail__send_message").await,   GateResult::Require));
        // Nor another session's calls (the map is keyed by conversation).
        let other = mgr
            .check(2, None, "assistant", "web", "mcp__gmail__modify_message", &json!({}), Some("default"))
            .await;
        assert!(matches!(other, GateResult::Require));

        // The connector-wide scope still exists for a caller that names it.
        mgr.bypass_session_for_mcp(1, "gmail".into(), None).await;
        assert!(matches!(decide("mcp__gmail__send_message").await, GateResult::Allow));

        db.close().await;
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path_str}{suffix}"));
        }
    }

    // End-to-end: run the real startup pipeline (migrate → seed) against a temp SQLite
    // DB pre-loaded with legacy rules, then assert the gate decisions through `check()`.
    #[tokio::test]
    async fn fs_seed_migrate_and_gate_end_to_end() {
        use super::{ApprovalManager, GateResult};
        use serde_json::json;
        use std::sync::Arc;
        use tokio::sync::broadcast;

        let path = std::env::temp_dir().join(format!("skald_fs_test_{}.db", std::process::id()));
        let path_str = path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&path);
        let pool = crate::db::init_system_pool(&path_str).await.expect("init_system_pool");
        let db = Arc::new(pool);

        // The `default` permission group is a FK target for approval_rules.group_id.
        sqlx::query("INSERT INTO tool_permission_groups (id, name) VALUES ('default', 'Default')")
            .execute(db.as_ref())
            .await
            .unwrap();

        // Pre-load LEGACY rows as an older install would have them.
        for t in ["write_file", "edit_file", "insert_at_line", "replace_lines"] {
            sqlx::query(
                "INSERT INTO approval_rules (tool_pattern, action, note, priority, group_id)
                 VALUES (?, 'require', 'default rule', 10, 'default')",
            )
            .bind(t)
            .execute(db.as_ref())
            .await
            .unwrap();
        }
        for t in ["write_file", "edit_file"] {
            sqlx::query(
                "INSERT INTO approval_rules (tool_pattern, path_pattern, action, note, priority, group_id)
                 VALUES (?, 'data/*', 'allow', 'auto-allow data/ writes', 5, 'default')",
            )
            .bind(t)
            .execute(db.as_ref())
            .await
            .unwrap();
        }
        for t in ["read_file", "grep_files"] {
            sqlx::query(
                "INSERT INTO approval_rules (tool_pattern, path_pattern, action, note, priority, group_id)
                 VALUES (?, 'secrets', 'deny', 'deny reading secrets/', 5, 'default')",
            )
            .bind(t)
            .execute(db.as_ref())
            .await
            .unwrap();
        }

        let (tx, _rx) = broadcast::channel(16);
        let mgr = ApprovalManager::new(Arc::clone(&db), tx);

        // Startup pipeline order (mirrors bundles.rs).
        mgr.seed_defaults().await.unwrap(); // no-op: table already non-empty
        mgr.migrate_legacy_fs_rules().await.unwrap();
        mgr.seed_fs_path_rules().await.unwrap();
        mgr.seed_default_catch_all().await.unwrap();

        // Legacy per-tool fs rows are migrated away…
        let legacy: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM approval_rules
             WHERE note IN ('default rule', 'auto-allow data/ writes', 'deny reading secrets/',
                            'deny secrets/ access')",
        )
        .fetch_one(db.as_ref())
        .await
        .unwrap();
        assert_eq!(legacy, 0, "legacy fs rules should be removed by migration");

        // …and replaced by exactly the six @fs_* token rows (shared-memory has two:
        // read-allow and write-require; plus user-memory, data, projects, and the
        // read-only skills tree).
        let fs_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM approval_rules WHERE tool_pattern LIKE '@fs%'",
        )
        .fetch_one(db.as_ref())
        .await
        .unwrap();
        assert_eq!(fs_rows, 6, "user-memory + shared-memory(r/w) + data + projects + skills @fs_* rules should be seeded");

        // Gate decisions through the real check() path.
        async fn decide(mgr: &ApprovalManager, tool: &str, path: &str) -> GateResult {
            mgr.check(1, None, "assistant", "web", tool, &json!({ "path": path }), Some("default")).await
        }
        // user-memory auto-allows reads and writes; the old `memory/*` no longer matches.
        assert!(matches!(decide(&mgr, "write_file", "user-memory/notes.md").await,    GateResult::Allow));
        assert!(matches!(decide(&mgr, "list_files", "user-memory").await,             GateResult::Allow));
        assert!(matches!(decide(&mgr, "write_file", "memory/notes.md").await,         GateResult::Require));
        // shared-memory: reads allowed, writes require approval.
        assert!(matches!(decide(&mgr, "read_file",  "shared-memory/casa.md").await,   GateResult::Allow));
        assert!(matches!(decide(&mgr, "write_file", "shared-memory/casa.md").await,   GateResult::Require));
        // Reading a skill never raises a card: the trust decision was taken when it
        // was installed. A write does not need a rule — the tree is read-only in
        // both directions — so it simply falls through to the catch-all.
        assert!(matches!(decide(&mgr, "read_file",  "skills/shared/ics/SKILL.md").await, GateResult::Allow));
        assert!(matches!(decide(&mgr, "list_files", "skills/daniele").await,              GateResult::Allow));
        assert!(matches!(decide(&mgr, "write_file", "skills/shared/ics/SKILL.md").await,  GateResult::Require));
        assert!(matches!(decide(&mgr, "edit_file",  "shared-memory/casa.md").await,   GateResult::Require));
        // The shared audit log is the one exception, and only for `append_file` — the
        // one write tool that cannot shorten a file. Its lower priority number must
        // beat the `@fs_write shared-memory/* require` rule.
        assert!(matches!(decide(&mgr, "append_file", "shared-memory/log.md").await,   GateResult::Allow));
        // …and the exception is narrow in both directions: another tool on the same
        // path, or the same tool on another shared note, still needs a human.
        assert!(matches!(decide(&mgr, "write_file",  "shared-memory/log.md").await,   GateResult::Require));
        assert!(matches!(decide(&mgr, "edit_file",   "shared-memory/log.md").await,   GateResult::Require));
        assert!(matches!(decide(&mgr, "append_file", "shared-memory/casa.md").await,  GateResult::Require));
        // Private memory is frictionless throughout, log included.
        assert!(matches!(decide(&mgr, "append_file", "user-memory/log.md").await,     GateResult::Allow));
        assert!(matches!(decide(&mgr, "read_file",  "data/x.txt").await,              GateResult::Allow));
        // project folders auto-allow reads and writes (subtree match on projects/*).
        assert!(matches!(decide(&mgr, "write_file", "projects/alice/budget/x.md").await, GateResult::Allow));
        assert!(matches!(decide(&mgr, "read_file",  "projects/alice/budget").await,      GateResult::Allow));
        // memory_search is allowed by a path-less tool rule (it has `query`, not `path`).
        assert!(matches!(decide(&mgr, "memory_search", "ignored").await,              GateResult::Allow));
        // The on-disk secrets store is gone, and with it its blanket deny: `secrets/`
        // is now an ordinary path in the caller's own home, gated by the catch-all
        // like any other. Denying it would deny a user their own folder.
        assert!(matches!(decide(&mgr, "read_file",  "secrets/key").await,     GateResult::Require));
        // Unmatched write falls through to the `*` catch-all.
        assert!(matches!(decide(&mgr, "write_file", "src/main.rs").await,     GateResult::Require));
        // Non-filesystem tool: unaffected by @fs_* rules, gated by catch-all.
        let cmd = mgr
            .check(1, None, "assistant", "web", "execute_cmd", &json!({ "command": "ls" }), Some("default"))
            .await;
        assert!(matches!(cmd, GateResult::Require));

        db.close().await;
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{path_str}{suffix}"));
        }
    }
}
