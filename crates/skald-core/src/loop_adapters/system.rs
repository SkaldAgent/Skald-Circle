//! `AgentSystemContext` — **every layer of Skald's system prompt**, as a
//! `SystemContextSource` (blueprint §10). It owns the content; the crate's
//! projection decides where each layer lands on the wire:
//!
//! | layer | wire position |
//! |---|---|
//! | AGENT.md + `inject_memory` + skills index + `extra_system` + substitutions | `base` — the cacheable prefix |
//! | session scratchpad | `extra_static` — a system message before the conversation |
//! | Honcho memory / per-turn overrides, then the date/time block | `dynamic_tail` — joined into the trailing system message |
//! | trailing reminder | `tail_reminder` |

use std::collections::HashMap;
use std::sync::Arc;

use agent_loop::context::{SystemContext, SystemContextSource, TurnInfo};
use sqlx::SqlitePool;

use crate::config::DatetimeConfig;
use crate::mcp::McpProvider;

/// Registry of installed skills, relative to Skald's process cwd. Injected
/// into agents that have `inject_skills` enabled (the default).
const SKILLS_INDEX_PATH: &str = "skills/index.md";

/// The static system content of one agent, resolved per turn.
pub struct AgentSystemContext {
    pub agent_id:      String,
    /// Static extra context (interface formatting rules, e.g. Telegram HTML).
    pub extra_static:  Option<String>,
    /// Dynamic extra context (Honcho memory merged with per-turn overrides),
    /// emitted as the dynamic tail.
    pub extra_dynamic: Option<String>,
    pub tail_reminder: Option<String>,
    pub substitutions: HashMap<String, String>,
    /// Owner pool (`user-memory/` notes).
    pub pool:          Arc<SqlitePool>,
    /// Shared pool (`shared-memory/`, shared folders, user profile).
    pub shared_pool:   Arc<SqlitePool>,
    pub user_id:       String,
    pub mcp:           Arc<dyn McpProvider>,
    /// Project root for `__PROJECT_ROOT__` expansion in `inject_memory`.
    pub project_root:  Option<String>,
    /// Scratchpad scope: the session's own id, or the parent's for an async
    /// sub-task (the blackboard is shared by every agent of a session).
    pub scratchpad_sid: i64,
    pub datetime:      DatetimeConfig,
}

#[agent_loop::async_trait]
impl SystemContextSource for AgentSystemContext {
    async fn system_context(&self, _turn: &TurnInfo) -> agent_loop::Result<SystemContext> {
        let mut static_content = crate::agents::load_prompt(&self.agent_id)?;

        let meta = crate::agents::load_meta(&self.agent_id)?;
        if !meta.inject_memory.is_empty() {
            static_content.push_str(
                "\n\n---\nThe following memory files have been loaded automatically. \
                 You can edit them with `edit_file` or `write_file` using the path shown.\n"
            );
            for mem_path in &meta.inject_memory {
                let (content, display) = self.load_inject_memory(mem_path).await;
                match content {
                    Some(c) => static_content.push_str(&format!(
                        "\n<memory_file path=\"{display}\">\n{c}\n</memory_file>\n"
                    )),
                    None => static_content.push_str(&format!(
                        "\n<memory_file path=\"{display}\">\n(file not created yet)\n</memory_file>\n"
                    )),
                }
            }
        }

        // Skills index — injected unless the agent opts out. Skipped silently
        // when no skills are installed.
        if meta.inject_skills {
            let (abs, display) = self.resolve_memory_path(SKILLS_INDEX_PATH);
            if let Ok(c) = tokio::fs::read_to_string(&abs).await {
                static_content.push_str(&format!(
                    "\n\n---\nInstalled skills you can use (read the linked `SKILL.md` before running a skill):\n\
                     \n<skills_index path=\"{display}\">\n{c}\n</skills_index>\n"
                ));
            }
        }

        if let Some(extra) = &self.extra_static {
            static_content.push_str("\n\n---\n");
            static_content.push_str(extra);
        }

        if static_content.contains("__MCP_LIST__") {
            static_content = static_content.replace("__MCP_LIST__", &self.render_mcp_list());
        }
        if static_content.contains("__SHARED_FOLDERS__") {
            static_content = static_content.replace(
                "__SHARED_FOLDERS__",
                &render_shared_folders_section(&self.shared_pool, &self.user_id).await?,
            );
        }
        if static_content.contains("__USER_PROFILE__") {
            static_content = static_content.replace(
                "__USER_PROFILE__",
                &render_user_profile_section(&self.shared_pool, &self.user_id).await?,
            );
        }

        for (key, value) in &self.substitutions {
            let sentinel = format!("__{key}__");
            if static_content.contains(sentinel.as_str()) {
                static_content = static_content.replace(sentinel.as_str(), value);
            }
        }

        // The scratchpad sits before the conversation: shared by every agent of
        // the session, and re-read every turn (it changes, so it is its own
        // message rather than part of the cached prefix).
        let extra_static = self.scratchpad_block().await?.into_iter().collect();

        // The fresh layers, in the order the model reads them.
        let mut dynamic_tail: Vec<String> = Vec::new();
        dynamic_tail.extend(self.extra_dynamic.clone());
        dynamic_tail.extend(self.datetime_block());

        Ok(SystemContext {
            base: static_content,
            extra_static,
            dynamic_tail,
            tail_reminder: self.tail_reminder.clone(),
        })
    }
}

/// OS description (type + version), computed once.
fn os_description() -> &'static str {
    static OS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    OS.get_or_init(|| os_info::get().to_string())
}

/// System IANA timezone name, computed once.
fn system_timezone() -> Option<&'static str> {
    static TZ: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TZ.get_or_init(|| iana_time_zone::get_timezone().ok()).as_deref()
}

impl AgentSystemContext {
    /// The session scratchpad as an XML block, or `None` when empty.
    async fn scratchpad_block(&self) -> agent_loop::Result<Option<String>> {
        let notes = crate::db::scratchpad::for_session(&self.pool, self.scratchpad_sid).await?;
        if notes.is_empty() {
            return Ok(None);
        }
        let mut s = String::from(
            "<scratchpad>\n  \
             <!-- Temporary notes shared by all agents in this session. Not persisted across sessions. -->\n",
        );
        for (k, v) in &notes {
            s.push_str(&format!("  <note key=\"{k}\">{v}</note>\n"));
        }
        s.push_str("</scratchpad>");
        Ok(Some(s))
    }

    /// The current date/time + OS + cwd block (`None` when disabled).
    ///
    /// Rounding exists for the prompt cache: a timestamp that changes every
    /// second would invalidate any cached suffix, so the instance can quantize
    /// it (this block is in the dynamic tail, after the cached prefix, but the
    /// rounding still helps providers that cache further).
    fn datetime_block(&self) -> Option<String> {
        if !self.datetime.enabled {
            return None;
        }
        let secs = chrono::Utc::now().timestamp();
        let secs = match self.datetime.round_minutes {
            Some(m) if m > 0 => {
                let bucket = (m as i64) * 60;
                (secs / bucket) * bucket
            }
            _ => secs,
        };

        let tz = self
            .datetime
            .timezone
            .as_deref()
            .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
            .or_else(|| system_timezone().and_then(|s| s.parse::<chrono_tz::Tz>().ok()));

        let (formatted, tz_name) = match tz {
            Some(tz) => {
                use chrono::TimeZone as _;
                let f = tz
                    .timestamp_opt(secs, 0)
                    .single()
                    .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%:z").to_string())
                    .unwrap_or_else(|| {
                        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
                    });
                (f, Some(tz.name().to_string()))
            }
            None => {
                let f = chrono::DateTime::from_timestamp(secs, 0)
                    .map(|utc| {
                        utc.with_timezone(&chrono::Local)
                            .format("%Y-%m-%dT%H:%M:%S%:z")
                            .to_string()
                    })
                    .unwrap_or_else(|| {
                        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
                    });
                (f, None)
            }
        };
        let date_line = match tz_name {
            Some(name) => format!("Current date and time: {formatted} ({name})"),
            None => format!("Current date and time: {formatted}"),
        };

        // The agent's cwd is always its container home.
        let cwd = "~";

        Some(format!(
            "{date_line}\nOperating system: {}\nWorking directory: {cwd}\n\
             Filesystem tools and execute_cmd resolve relative paths against your home directory.",
            os_description()
        ))
    }

    /// Loads an `inject_memory` entry, returning `(content, display_path)`.
    /// Virtual memory paths read from SQLite; everything else is a disk read.
    async fn load_inject_memory(&self, mem_path: &str) -> (Option<String>, String) {
        use crate::tools::fs::{MemScope, classify_memory};
        if let Some(m) = classify_memory(mem_path) {
            let pool = match m.scope {
                MemScope::User   => &self.pool,
                MemScope::Shared => &self.shared_pool,
            };
            let content = crate::db::memory_docs::get(pool, &m.rel)
                .await.ok().flatten().map(|d| d.content);
            return (content, mem_path.to_string());
        }
        let (abs, display) = self.resolve_memory_path(mem_path);
        (tokio::fs::read_to_string(&abs).await.ok(), display)
    }

    fn resolve_memory_path(&self, mem_path: &str) -> (std::path::PathBuf, String) {
        let display = if mem_path.contains("__PROJECT_ROOT__") {
            match &self.project_root {
                Some(root) => mem_path.replace("__PROJECT_ROOT__", root),
                None => {
                    tracing::warn!(
                        mem_path,
                        "inject_memory entry references __PROJECT_ROOT__ but this session has no project root; skipping"
                    );
                    return (std::path::PathBuf::from(mem_path), mem_path.to_string());
                }
            }
        } else {
            mem_path.to_string()
        };
        let abs = crate::tools::fs::resolve(&display)
            .unwrap_or_else(|_| std::path::PathBuf::from(&display));
        (abs, display)
    }

    /// The **static** catalogue of loadable MCP servers (identical regardless
    /// of which are active — cache-prefix stability).
    fn render_mcp_list(&self) -> String {
        let all_servers: std::collections::BTreeSet<String> = self.mcp.tools()
            .into_iter()
            .map(|t| t.server_name)
            .collect();

        if all_servers.is_empty() {
            return String::new();
        }

        let descriptions = self.mcp.server_descriptions();

        let mut out = String::from(
            "## MCP servers\n\nConnectors you can load with `activate_tools([\"name\"])`. \
             Once loaded, a server's tools are callable as `mcp__<name>__<tool>`:\n\n",
        );
        out.push_str("| Server | Description |\n|--------|-------------|\n");
        for name in &all_servers {
            let desc = descriptions.get(name)
                .and_then(|d| d.as_deref())
                .unwrap_or("—");
            out.push_str(&format!("| `{name}` | {desc} |\n"));
        }
        out
    }
}

// ── Prompt sections resolved from the registry ───────────────────────────────


/// `__SHARED_FOLDERS__` section, resolved from the registry (shared with the
/// `agent-loop` adapter's system-context source).
pub(crate) async fn render_shared_folders_section(
    shared_pool: &SqlitePool,
    user_id:     &str,
) -> anyhow::Result<String> {
    let rows = crate::db::shared_folders::agent_view(shared_pool, user_id).await?;
    Ok(render_shared_folders_table(&rows))
}

/// `__USER_PROFILE__` block, resolved from the registry (shared with the
/// `agent-loop` adapter's system-context source).
pub(crate) async fn render_user_profile_section(
    shared_pool: &SqlitePool,
    user_id:     &str,
) -> anyhow::Result<String> {
    let user = crate::db::users::get(shared_pool, user_id).await?;
    let locale = crate::i18n::resolve_locale(
        shared_pool,
        user.as_ref().and_then(|u| u.locale.as_deref()),
    ).await;
    Ok(render_user_profile_block(
        user.as_ref(),
        &locale,
        chrono::Utc::now().date_naive(),
    ))
}

/// Renders the shared-folders section body as a Markdown table — one row per
/// folder the user belongs to, naming the folder's other members so the model
/// knows exactly who sees what is written there. An empty membership yields an
/// explicit "not a member" line so the model does not go probing `shared/` paths.
fn render_shared_folders_table(rows: &[crate::db::shared_folders::SharedFolderAccess]) -> String {    /// A free-text cell: single line, pipes escaped (they would split the table).
    fn cell(s: &str) -> String {
        s.trim().replace('|', "\\|").replace('\n', " ")
    }
    if rows.is_empty() {
        return "_You are not a member of any shared folder._\n".to_string();
    }
    let mut out = String::from("| Path | Access | Shared with | Description |\n|------|--------|-------------|-------------|\n");
    for r in rows {
        let access = if r.can_write { "read-write" } else { "read-only" };
        let shared_with = if r.shared_with.is_empty() { "—".to_string() } else { cell(&r.shared_with) };
        let desc = if r.description.trim().is_empty() { "—".to_string() } else { cell(&r.description) };
        out.push_str(&format!("| `shared/{}` | {access} | {shared_with} | {desc} |\n", r.folder_name));
    }
    out
}

/// Renders the profile block for `__USER_PROFILE__`. Every line is always
/// present — an explicit `unknown` / `not specified` is a signal the agent can
/// act on (e.g. gently ask) — except `Notes`, omitted entirely when empty.
/// `today` is passed in so the age computation stays pure and testable.
fn render_user_profile_block(
    user:   Option<&crate::db::users::User>,
    locale: &str,
    today:  chrono::NaiveDate,
) -> String {
    let name = user
        .and_then(|u| non_empty(&u.display_name))
        .or_else(|| user.map(|u| u.username.as_str()))
        .unwrap_or("unknown");

    let birth = match user.and_then(|u| non_empty(&u.birthdate)) {
        Some(raw) => match chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
            Ok(dob) => match today.years_since(dob) {
                Some(age) => format!("{raw} (age {age})"),
                None      => format!("{raw} (age unknown)"),
            },
            // Stored value bypassed validation — show it raw rather than drop it.
            Err(_) => raw.to_string(),
        },
        None => "unknown".to_string(),
    };

    let sex = user.and_then(|u| non_empty(&u.sex)).unwrap_or("not specified");

    let mut out = format!(
        "Name: {name}\nDate of birth: {birth}\nSex: {sex}\nPreferred language: {}\n",
        crate::i18n::language_name(locale),
    );
    if let Some(notes) = user.and_then(|u| non_empty(&u.notes)) {
        out.push_str(&format!("Notes: {notes}\n"));
    }
    out
}

/// An optional string field as a trimmed `&str`, `None` when empty/blank.
fn non_empty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_folders_table_renders_access_and_description() {
        use crate::db::shared_folders::SharedFolderAccess;
        let rows = vec![
            SharedFolderAccess { folder_name: "photos".into(), can_write: false, shared_with: "Bob, Carol".into(), description: "Shared photo archive".into() },
            SharedFolderAccess { folder_name: "recipes".into(), can_write: true, shared_with: "".into(), description: "a | b\nc".into() },
        ];
        let out = render_shared_folders_table(&rows);
        assert!(out.starts_with("| Path | Access | Shared with | Description |\n|------|--------|-------------|-------------|\n"));
        assert!(out.contains("| `shared/photos` | read-only | Bob, Carol | Shared photo archive |\n"));
        // Empty shared_with → "—"; free-text cells stay on one line with escaped pipes.
        assert!(out.contains("| `shared/recipes` | read-write | — | a \\| b c |\n"));
    }

    #[test]
    fn shared_folders_table_empty_membership_is_explicit() {
        assert_eq!(
            render_shared_folders_table(&[]),
            "_You are not a member of any shared folder._\n"
        );
    }

    fn test_user() -> crate::db::users::User {
        crate::db::users::User {
            id:           "u-1".into(),
            username:     "luca".into(),
            display_name: None,
            role_id:      "members".into(),
            credentials:  crate::db::users::Credentials::Cleartext(None),
            active:       true,
            locale:       None,
            birthdate:    None,
            sex:          None,
            notes:        None,
            created_at:   "now".into(),
            updated_at:   "now".into(),
        }
    }

    #[test]
    fn user_profile_renders_all_fields_with_runtime_age() {
        let mut u = test_user();
        u.display_name = Some("Luca Rossi".into());
        u.birthdate    = Some("2019-02-10".into());
        u.sex          = Some("male".into());
        u.notes        = Some("loves dinosaurs".into());
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();

        let out = render_user_profile_block(Some(&u), "it", today);
        assert_eq!(
            out,
            "Name: Luca Rossi\n\
             Date of birth: 2019-02-10 (age 7)\n\
             Sex: male\n\
             Preferred language: Italian\n\
             Notes: loves dinosaurs\n"
        );
    }

    #[test]
    fn user_profile_age_counts_uncelebrated_birthdays() {
        let mut u = test_user();
        u.birthdate = Some("2019-12-25".into());
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let out = render_user_profile_block(Some(&u), "en", today);
        assert!(out.contains("Date of birth: 2019-12-25 (age 6)\n"), "{out}");
    }

    #[test]
    fn user_profile_empty_fields_are_explicit_and_notes_omitted() {
        let u = test_user();
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let out = render_user_profile_block(Some(&u), "en", today);
        assert_eq!(
            out,
            "Name: luca\n\
             Date of birth: unknown\n\
             Sex: not specified\n\
             Preferred language: English\n"
        );
    }

    #[test]
    fn user_profile_tolerates_garbage_and_future_dates() {
        let mut u = test_user();
        u.birthdate = Some("not-a-date".into());
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let out = render_user_profile_block(Some(&u), "en", today);
        assert!(out.contains("Date of birth: not-a-date\n"), "{out}");

        u.birthdate = Some("2099-01-01".into());
        let out = render_user_profile_block(Some(&u), "en", today);
        assert!(out.contains("Date of birth: 2099-01-01 (age unknown)\n"), "{out}");
    }

    #[test]
    fn user_profile_missing_user_still_renders_language() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 7, 18).unwrap();
        let out = render_user_profile_block(None, "fr", today);
        assert_eq!(
            out,
            "Name: unknown\n\
             Date of birth: unknown\n\
             Sex: not specified\n\
             Preferred language: French\n"
        );
    }
}
