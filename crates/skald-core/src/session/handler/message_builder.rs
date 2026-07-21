use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::compactor::{ContextCompactor, SUMMARY_PREFIX};
use crate::config::DatetimeConfig;
use crate::db::{chat_history, chat_llm_tools, chat_summaries};
use crate::mcp::McpProvider;
use crate::tools::tool_names as tn;

/// Registry of installed skills, relative to Skald's process cwd. Injected into agents
/// that have `inject_skills` enabled (the default).
const SKILLS_INDEX_PATH: &str = "skills/index.md";

/// OS description (type + version), computed once — it does not change at runtime.
fn os_description() -> &'static str {
    static OS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    OS.get_or_init(|| os_info::get().to_string())
}

/// System IANA timezone name (e.g. `Europe/Rome`), computed once. `None` if it can't
/// be determined.
fn system_timezone() -> Option<&'static str> {
    static TZ: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    TZ.get_or_init(|| iana_time_zone::get_timezone().ok()).as_deref()
}

/// Pure service that builds the OpenAI-format message array for one LLM round.
///
/// Extracting this from `ChatSessionHandler` allows the builder to be constructed
/// and called in isolation (e.g. in integration tests with an in-memory SQLite DB)
/// without needing the full handler and all its dependencies.
pub struct MessageBuilder {
    pub pool:                  Arc<SqlitePool>,
    /// The shared (`system.db`) pool, for injecting `shared-memory/` notes. The
    /// owner `pool` above backs `user-memory/`.
    pub shared_pool:           Arc<SqlitePool>,
    /// The authenticated user who owns this session — drives per-user prompt
    /// sections like the `__SHARED_FOLDERS__` table (registry read on
    /// `shared_pool`).
    pub user_id:               String,
    pub session_id:            i64,
    pub mcp:                   Arc<dyn McpProvider>,
    pub datetime_config:       DatetimeConfig,
    pub max_history_messages:  usize,
    pub max_tool_result_chars: Option<usize>,
    pub compactor:             Option<Arc<ContextCompactor>>,
    /// Project root (agent path `projects/{owner}/{slug}`) when this is a project
    /// session — used to resolve `__PROJECT_ROOT__` placeholders in `inject_memory`
    /// paths. `None` for non-project sessions, in which case an `inject_memory`
    /// entry that references `__PROJECT_ROOT__` is skipped (with a warning).
    pub project_root:          Option<String>,
}

impl MessageBuilder {
    /// Builds a raw OpenAI-format message array from the persisted history,
    /// reconstructing assistant tool-call entries and tool-result entries from
    /// the `chat_llm_tools` table.
    ///
    /// `active_mcp_grants` is the set of MCP server names currently granted for
    /// this session. It is used to build the compact MCP availability list injected
    /// into the system prompt so the LLM knows which servers it can activate.
    ///
    /// ## Message order (optimised for prefix KV caching)
    ///
    /// ```text
    /// 1. [system]  Static content — AGENT.md + memory files + extra_system_static + MCP list
    ///              Tagged cache_control:ephemeral when cache_hints=true (Anthropic via OpenRouter).
    ///
    /// 2. [system]  Scratchpad — emitted only when non-empty, BEFORE the conversation.
    ///
    /// 3. [system]  Compaction summary — if a summary exists for this stack.
    ///
    /// 4. [user / assistant / tool]  Conversation history.
    ///
    /// 5. [system]  Dynamic tail — extra_system_dynamic + current date/time/OS/cwd.
    ///
    /// 6. [system]  Tail reminder — short anti-drift reminder (e.g. Telegram format).
    /// ```
    pub async fn build(
        &self,
        stack_id:             i64,
        agent_id:             &str,
        extra_system_static:  Option<&str>,
        extra_system_dynamic: Option<&str>,
        tail_reminder:        Option<&str>,
        active_mcp_grants:    &HashSet<String>,
        system_substitutions: &HashMap<String, String>,
        cache_hints:          bool,
        // Input capabilities of the resolved model (`vision`, `video`, …) —
        // drives inline media for current-turn attachments.
        capabilities:         &[String],
    ) -> anyhow::Result<Vec<Value>> {
        let pool = &*self.pool;

        // ── 1. Static system message ──────────────────────────────────────────
        let mut static_content = crate::agents::load_prompt(agent_id)?;

        let meta = crate::agents::load_meta(agent_id)?;
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

        // ── Skills index ──────────────────────────────────────────────────────
        // Injected for every agent unless it opts out (`inject_skills: false`).
        // Reuses the memory-path resolver for display consistency. Skipped silently
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

        if let Some(extra) = extra_system_static {
            static_content.push_str("\n\n---\n");
            static_content.push_str(extra);
        }

        if static_content.contains("__MCP_LIST__") {
            static_content = static_content.replace(
                "__MCP_LIST__",
                &self.render_mcp_list(active_mcp_grants),
            );
        }

        if static_content.contains("__SHARED_FOLDERS__") {
            static_content = static_content.replace(
                "__SHARED_FOLDERS__",
                &self.render_shared_folders().await?,
            );
        }

        if static_content.contains("__USER_PROFILE__") {
            static_content = static_content.replace(
                "__USER_PROFILE__",
                &self.render_user_profile().await?,
            );
        }

        for (key, value) in system_substitutions {
            let sentinel = format!("__{key}__");
            if static_content.contains(sentinel.as_str()) {
                static_content = static_content.replace(sentinel.as_str(), value);
            }
        }

        let static_msg = if cache_hints {
            json!({
                "role": "system",
                "content": [{ "type": "text", "text": static_content, "cache_control": { "type": "ephemeral" } }]
            })
        } else {
            json!({ "role": "system", "content": static_content })
        };

        let mut out = vec![static_msg];

        // ── 2. Scratchpad system message (before conversation) ────────────────
        let scratch = crate::db::scratchpad::for_session(pool, self.session_id).await?;
        if !scratch.is_empty() {
            let mut s = String::from(
                "<scratchpad>\n  \
                 <!-- Temporary notes shared by all agents in this session. Not persisted across sessions. -->\n"
            );
            for (k, v) in &scratch {
                s.push_str(&format!("  <note key=\"{k}\">{v}</note>\n"));
            }
            s.push_str("</scratchpad>");
            out.push(json!({ "role": "system", "content": s }));
        }

        // ── 3. Context compaction: inject summary + load messages after boundary ──
        let summary = chat_summaries::latest_for_stack(pool, stack_id).await?;
        let mut history = match &summary {
            Some(s) => {
                out.push(json!({
                    "role": "system",
                    "content": format!(
                        "{SUMMARY_PREFIX}\n\n{}\n\n\
                         [End of context summary — the following messages are the most recent exchanges in full.]",
                        s.content
                    )
                }));
                chat_history::for_stack_since(pool, stack_id, s.covers_up_to_message_id).await?
            }
            None => chat_history::for_stack(pool, stack_id).await?,
        };

        if self.compactor.is_none() && history.len() > self.max_history_messages {
            history.drain(..history.len() - self.max_history_messages);
            if matches!(history.first().map(|m| &m.role), Some(chat_history::Role::Assistant)) {
                history.drain(..1);
            }
        }

        let current_turn_boundary = history
            .iter()
            .rposition(|e| matches!(e.role, chat_history::Role::User | chat_history::Role::Agent));

        // Inline-media turn group. Trailing assistant rows are the in-flight
        // turn's own rounds (their tool calls are already persisted), so the
        // current turn's user messages sit just before them; a coalesced run of
        // user/agent rows ahead of those belongs to the same turn. Media from
        // earlier turns degrades to the textual path block — re-sending images
        // on every round would re-bill them each time.
        let mut media_turn_start = history.len();
        while media_turn_start > 0
            && matches!(history[media_turn_start - 1].role, chat_history::Role::Assistant)
        {
            media_turn_start -= 1;
        }
        while media_turn_start > 0
            && matches!(
                history[media_turn_start - 1].role,
                chat_history::Role::User | chat_history::Role::Agent
            )
        {
            media_turn_start -= 1;
        }

        for (idx, entry) in history.iter().enumerate() {
            let is_previous_turn = current_turn_boundary.is_some_and(|b| idx < b);

            match entry.role {
                chat_history::Role::User | chat_history::Role::Agent => {
                    // Attachments reach the model two ways: media of the current
                    // turn is inlined as native content parts when the resolved
                    // model declares the capability (media::partition); everything
                    // else — and every attachment of older turns — keeps the
                    // textual path block, generated on the fly and never
                    // persisted as content.
                    let (text, media) = match &entry.metadata {
                        Some(meta) if !meta.attachments.is_empty() && idx >= media_turn_start => {
                            let partition = super::media::partition(&meta.attachments, capabilities).await;
                            (
                                format!(
                                    "{}{}",
                                    entry.content,
                                    core_api::message_meta::attachments_block(&partition.rest),
                                ),
                                partition.parts,
                            )
                        }
                        Some(meta) if !meta.attachments.is_empty() => (
                            format!(
                                "{}{}",
                                entry.content,
                                core_api::message_meta::attachments_block(&meta.attachments),
                            ),
                            Vec::new(),
                        ),
                        _ => (entry.content.clone(), Vec::new()),
                    };
                    // Coalesce consecutive user/agent rows into a single `role:user`
                    // turn. The DB keeps each message as its own row (distinct bubbles,
                    // per-message attachments), but the model must see one clean user
                    // turn — e.g. when several messages were injected back-to-back at a
                    // round boundary, or queued together while idle. `for_stack` already
                    // excludes `failed` rows, so only non-failed messages merge here.
                    push_user_chunk(&mut out, text, media);
                }
                chat_history::Role::Assistant => {
                    let tool_calls = chat_llm_tools::for_message(pool, entry.id).await?;

                    if tool_calls.is_empty() {
                        let mut msg = json!({ "role": "assistant", "content": entry.content });
                        if let Some(rc) = &entry.reasoning_content {
                            // Echo under both names: DeepSeek expects "reasoning_content",
                            // MiniMax M3 and others expect "reasoning".
                            msg["reasoning_content"] = rc.clone().into();
                            msg["reasoning"]         = rc.clone().into();
                        }
                        out.push(msg);
                    } else {
                        let tc_array: Vec<Value> = tool_calls
                            .iter()
                            .map(|tc| json!({
                                "id":   format!("tc_{}", tc.id),
                                "type": "function",
                                "function": {
                                    "name":      tc.name,
                                    "arguments": tc.arguments.as_deref().unwrap_or("{}"),
                                }
                            }))
                            .collect();

                        let mut msg = json!({
                            "role":       "assistant",
                            "content":    entry.content,
                            "tool_calls": tc_array,
                        });
                        if let Some(rc) = &entry.reasoning_content {
                            // Echo under both names: DeepSeek expects "reasoning_content",
                            // MiniMax M3 and others expect "reasoning".
                            msg["reasoning_content"] = rc.clone().into();
                            msg["reasoning"]         = rc.clone().into();
                        }
                        out.push(msg);

                        for tc in &tool_calls {
                            let result_content = match tc.status.as_str() {
                                "done"   => tc.result.as_deref().unwrap_or("").to_string(),
                                "failed" => format!(
                                    "Error: {}",
                                    tc.result.as_deref().unwrap_or("unknown error")
                                ),
                                // A human/policy rejection or a /stop cancellation is a
                                // deliberate, terminal outcome — surface the saved reason
                                // (the user's justification) so the LLM understands the
                                // tool did NOT run and why, instead of retrying blindly.
                                "rejected" => tc.result.as_deref()
                                    .unwrap_or("User rejected this tool call.")
                                    .to_string(),
                                "cancelled" => tc.result.as_deref()
                                    .unwrap_or("Tool call was cancelled by the user.")
                                    .to_string(),
                                // 'pending'/'running' left behind by a crash or a lost
                                // connection: the call really was interrupted mid-flight.
                                _ => "Error: tool call was interrupted (connection lost before user approval). Please retry the operation.".to_string(),
                            };

                            let result_content = self.maybe_hide_tool_result(
                                result_content,
                                is_previous_turn,
                                &tc.name,
                                tc.arguments.as_deref(),
                            );

                            out.push(json!({
                                "role":         "tool",
                                "tool_call_id": format!("tc_{}", tc.id),
                                "content":      result_content,
                            }));
                        }
                    }
                }
            }
        }

        // ── 5. Dynamic tail system message (after conversation) ──────────────
        {
            let datetime_line = if self.datetime_config.enabled {
                let now_utc = chrono::Utc::now();
                let secs = now_utc.timestamp();

                let secs = match self.datetime_config.round_minutes {
                    Some(m) if m > 0 => {
                        let bucket = (m as i64) * 60;
                        (secs / bucket) * bucket
                    }
                    _ => secs,
                };

                // Effective timezone: the one configured in config.yml if set, else the
                // OS timezone. When resolvable we show the IANA name alongside the offset.
                let tz = self.datetime_config.timezone.as_deref()
                    .and_then(|s| s.parse::<chrono_tz::Tz>().ok())
                    .or_else(|| system_timezone().and_then(|s| s.parse::<chrono_tz::Tz>().ok()));

                let (formatted, tz_name) = match tz {
                    Some(tz) => {
                        use chrono::TimeZone as _;
                        let f = tz.timestamp_opt(secs, 0)
                            .single()
                            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%:z").to_string())
                            .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string());
                        (f, Some(tz.name().to_string()))
                    }
                    None => {
                        let f = chrono::DateTime::from_timestamp(secs, 0)
                            .map(|utc| utc.with_timezone(&chrono::Local).format("%Y-%m-%dT%H:%M:%S%:z").to_string())
                            .unwrap_or_else(|| chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string());
                        (f, None)
                    }
                };
                let date_line = match tz_name {
                    Some(name) => format!("Current date and time: {formatted} ({name})"),
                    None       => format!("Current date and time: {formatted}"),
                };

                let cwd = "~";

                Some(format!(
                    "{date_line}\nOperating system: {}\nWorking directory: {cwd}\n\
                     Filesystem tools and execute_cmd resolve relative paths against your home directory.",
                    os_description()
                ))
            } else {
                None
            };

            let tail = match (extra_system_dynamic, datetime_line.as_deref()) {
                (Some(dyn_ctx), Some(dt)) => Some(format!("{dyn_ctx}\n\n---\n{dt}")),
                (Some(dyn_ctx), None)     => Some(dyn_ctx.to_string()),
                (None,          Some(dt)) => Some(dt.to_string()),
                (None,          None)     => None,
            };
            if let Some(content) = tail {
                out.push(json!({ "role": "system", "content": content }));
            }
        }

        // ── 6. Tail reminder ──────────────────────────────────────────────────
        if let Some(reminder) = tail_reminder {
            out.push(json!({ "role": "system", "content": reminder }));
        }

        Ok(out)
    }

    /// Returns the tool result as-is, or replaces it with an informative 1-line
    /// summary when the result belongs to a previous turn and exceeds `max_tool_result_chars`.
    fn maybe_hide_tool_result(
        &self,
        result:           String,
        is_previous_turn: bool,
        tool_name:        &str,
        arguments:        Option<&str>,
    ) -> String {
        if !is_previous_turn {
            return result;
        }
        let Some(limit) = self.max_tool_result_chars else {
            return result;
        };
        if result.len() <= limit {
            return result;
        }
        summarize_tool_result(tool_name, arguments, &result)
    }

    /// Builds the MCP list section that replaces the `__MCP_LIST__` sentinel.
    /// Resolves an `inject_memory` entry to `(absolute path to read, path to show)`.
    ///
    /// `__PROJECT_ROOT__` expands to the session's project root (the agent path
    /// `projects/{owner}/{slug}`, set on the RunContext for project sessions) —
    /// e.g. `"__PROJECT_ROOT__/SKALD.md"` loads a project-local diary. The shown
    /// path is the agent path itself, which the loop's filesystem routing
    /// resolves back to the same file when the agent references it via
    /// `edit_file`/`write_file`.
    /// Loads an `inject_memory` entry, returning `(content, display_path)`.
    ///
    /// Virtual memory paths are read from SQLite: `user-memory/…` from the owner
    /// `pool`, `shared-memory/…` from the `shared_pool` (`system.db`). Everything
    /// else (`data/…`, `__PROJECT_ROOT__/…`, an absolute path) is an ordinary disk
    /// read. A missing note / file yields `None`, rendered as "(file not created yet)".
    async fn load_inject_memory(&self, mem_path: &str) -> (Option<String>, String) {
        use crate::tools::fs::{classify_memory, MemScope};
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

    /// Builds the shared-folders table that replaces the `__SHARED_FOLDERS__`
    /// sentinel: the folders the session's user belongs to, with their access
    /// level and the admin-authored description (registry tables on
    /// `shared_pool`).
    async fn render_shared_folders(&self) -> anyhow::Result<String> {
        let rows = crate::db::shared_folders::agent_view(&self.shared_pool, &self.user_id).await?;
        Ok(render_shared_folders_table(&rows))
    }

    /// Builds the user-profile block that replaces the `__USER_PROFILE__`
    /// sentinel: the session owner's admin-managed directory fields (registry
    /// `users` row on `shared_pool`), with the age computed at build time and
    /// the preferred language resolved through the standard chain
    /// (`users.locale` → instance default → English).
    async fn render_user_profile(&self) -> anyhow::Result<String> {
        let user = crate::db::users::get(&self.shared_pool, &self.user_id).await?;
        let locale = crate::i18n::resolve_locale(
            &self.shared_pool,
            user.as_ref().and_then(|u| u.locale.as_deref()),
        ).await;
        Ok(render_user_profile_block(
            user.as_ref(),
            &locale,
            chrono::Utc::now().date_naive(),
        ))
    }

    fn render_mcp_list(&self, active_mcp_grants: &HashSet<String>) -> String {
        let all_servers: std::collections::BTreeSet<String> = self.mcp.tools()
            .into_iter()
            .map(|t| t.server_name)
            .collect();

        if all_servers.is_empty() {
            return String::new();
        }

        let descriptions = self.mcp.server_descriptions();

        let hidden: Vec<&String> = all_servers.iter()
            .filter(|n| !active_mcp_grants.contains(*n))
            .collect();
        let active: Vec<&String> = all_servers.iter()
            .filter(|n| active_mcp_grants.contains(*n))
            .collect();

        let mut out = String::from("## MCP servers\n");

        if !hidden.is_empty() {
            out.push_str("\n**Available** — call `activate_tools([\"name\"])` to load tools:\n\n");
            out.push_str("| Server | Description |\n|--------|-------------|\n");
            for name in &hidden {
                let desc = descriptions.get(*name)
                    .and_then(|d| d.as_deref())
                    .unwrap_or("—");
                out.push_str(&format!("| `{name}` | {desc} |\n"));
            }
        }

        if !active.is_empty() {
            out.push_str("\n**Active** — tools callable as `mcp__<name>__<tool>`:\n");
            for name in &active {
                out.push_str(&format!("- `{name}`\n"));
            }
        }

        out
    }
}

// ── Free helpers ──────────────────────────────────────────────────────────────

/// Renders the shared-folders section body as a Markdown table — one row per
/// folder the user belongs to, naming the folder's other members so the model
/// knows exactly who sees what is written there. An empty membership yields an
/// explicit "not a member" line so the model does not go probing `shared/` paths.
fn render_shared_folders_table(rows: &[crate::db::shared_folders::SharedFolderAccess]) -> String {
    /// A free-text cell: single line, pipes escaped (they would split the table).
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

/// Appends one user/agent chunk — text plus any inline media parts — to the
/// message stream, coalescing with a preceding `user` message. Plain-text
/// chunks merge exactly as before (one string); when either side carries
/// parts, the merged content is normalized to a parts array, with the new
/// text folded into the LAST text part so media parts keep their position.
fn push_user_chunk(out: &mut Vec<Value>, text: String, media: Vec<Value>) {
    fn text_part(t: &str) -> Value {
        json!({ "type": "text", "text": t })
    }

    if let Some(last) = out.last_mut()
        && last["role"] == "user"
    {
        if !last["content"].is_array() && media.is_empty() {
            let prev = last["content"].as_str().unwrap_or("").to_string();
            last["content"] = Value::String(format!("{prev}\n\n{text}"));
            return;
        }
        let mut parts = match last["content"].take() {
            Value::Array(a)  => a,
            Value::String(s) => vec![text_part(&s)],
            _                => Vec::new(),
        };
        if let Some(tp) = parts.iter_mut().rev().find(|p| p["type"] == "text") {
            let prev = tp["text"].as_str().unwrap_or("").to_string();
            tp["text"] = Value::String(format!("{prev}\n\n{text}"));
        } else {
            parts.insert(0, text_part(&text));
        }
        parts.extend(media);
        last["content"] = Value::Array(parts);
        return;
    }
    if media.is_empty() {
        out.push(json!({ "role": "user", "content": text }));
    } else {
        let mut parts = vec![text_part(&text)];
        parts.extend(media);
        out.push(json!({ "role": "user", "content": parts }));
    }
}

/// Creates an informative 1-line summary of a tool call result.
///
/// Produces human-readable descriptions like:
/// ```text
/// [execute_cmd] ran `cargo build` → exit 0, 47 lines output
/// [read_file] read src/main.rs (3,200 chars)
/// [write_file] wrote to agents/foo/AGENT.md
/// ```
fn summarize_tool_result(tool_name: &str, arguments: Option<&str>, result: &str) -> String {
    let args: serde_json::Value = arguments
        .and_then(|a| serde_json::from_str(a).ok())
        .unwrap_or(serde_json::Value::Null);

    let char_count = result.len();
    let line_count = if result.trim().is_empty() { 0 } else { result.lines().count() };

    fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> &'a str {
        args[key].as_str().unwrap_or("?")
    }

    match tool_name {
        tn::EXECUTE_CMD => {
            let cmd = args["command"].as_str().unwrap_or("");
            let cmd_display = super::preview_truncate(cmd, 77);
            let exit_code = result
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("exit: "))
                .unwrap_or("?");
            format!("[execute_cmd] ran `{cmd_display}` → exit {exit_code}, {line_count} lines output")
        }

        "read_file" | "read_file_chunk" => {
            let path = arg_str(&args, "path");
            format!("[{tool_name}] read {path} ({char_count} chars)")
        }

        "write_file" => {
            let path = arg_str(&args, "path");
            format!("[write_file] wrote to {path}")
        }

        "edit_file" | "patch_file" => {
            let path = arg_str(&args, "path");
            format!("[{tool_name}] edited {path}")
        }

        "list_dir" | "glob" => {
            let path = args["path"].as_str()
                .or_else(|| args["pattern"].as_str())
                .unwrap_or("?");
            format!("[{tool_name}] {path} ({char_count} chars)")
        }

        "list_items" => {
            let kind = arg_str(&args, "type");
            format!("[list_items] {kind} ({char_count} chars)")
        }

        "toggle_item" => {
            let kind    = arg_str(&args, "kind");
            let id      = arg_str(&args, "id");
            let enabled = args["enabled"].as_bool().unwrap_or(false);
            format!("[toggle_item] {kind} '{id}' → {}", if enabled { "enabled" } else { "disabled" })
        }

        tn::READ_NOTIFICATION => {
            let count = serde_json::from_str::<Vec<serde_json::Value>>(result)
                .map(|v| v.len())
                .unwrap_or(0);
            format!("[read_notification] {count} notification(s)")
        }

        tn::EXECUTE_TASK | tn::EXECUTE_SUBTASK => {
            let agent = arg_str(&args, "agent_id");
            format!("[{tool_name}] → {agent} ({char_count} chars result)")
        }

        tn::ACTIVATE_TOOLS => {
            let groups = args["groups"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
                .unwrap_or_else(|| "?".to_string());
            format!("[activate_tools] loaded: {groups}")
        }

        _ if tool_name.starts_with("mcp__") => {
            format!("[{tool_name}] ({char_count} chars result)")
        }

        _ => {
            let first_arg = args.as_object()
                .and_then(|m| m.iter().next())
                .map(|(k, v)| {
                    let sv = super::preview_truncate(v.as_str().unwrap_or_default(), 40);
                    format!(" {k}={sv}")
                })
                .unwrap_or_default();
            format!("[{tool_name}]{first_arg} ({char_count} chars result)")
        }
    }
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

    fn img() -> Value {
        json!({ "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } })
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

    #[test]
    fn plain_text_chunks_merge_as_string() {
        let mut out = vec![];
        push_user_chunk(&mut out, "one".into(), vec![]);
        push_user_chunk(&mut out, "two".into(), vec![]);
        assert_eq!(out, vec![json!({ "role": "user", "content": "one\n\ntwo" })]);
    }

    #[test]
    fn media_chunk_normalizes_to_parts() {
        let mut out = vec![];
        push_user_chunk(&mut out, "look".into(), vec![img()]);
        assert_eq!(out, vec![json!({
            "role": "user",
            "content": [
                { "type": "text", "text": "look" },
                { "type": "image_url", "image_url": { "url": "data:image/png;base64,QUJD" } },
            ]
        })]);
    }

    #[test]
    fn text_after_media_folds_into_last_text_part() {
        let mut out = vec![];
        push_user_chunk(&mut out, "look".into(), vec![img()]);
        push_user_chunk(&mut out, "and this".into(), vec![]);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], json!("look\n\nand this"));
        assert_eq!(content[1]["type"], json!("image_url"));
    }

    #[test]
    fn media_merges_after_plain_text() {
        let mut out = vec![];
        push_user_chunk(&mut out, "one".into(), vec![]);
        push_user_chunk(&mut out, "two".into(), vec![img()]);
        let content = out[0]["content"].as_array().unwrap();
        assert_eq!(content[0]["text"], json!("one\n\ntwo"));
        assert_eq!(content[1]["type"], json!("image_url"));
    }

    #[test]
    fn chunk_after_assistant_starts_new_message() {
        let mut out = vec![json!({ "role": "assistant", "content": "hi" })];
        push_user_chunk(&mut out, "one".into(), vec![img()]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1]["role"], json!("user"));
        assert!(out[1]["content"].is_array());
    }
}
