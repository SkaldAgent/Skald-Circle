use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::SqlitePool;

use crate::compactor::{ContextCompactor, SUMMARY_PREFIX};
use crate::config::DatetimeConfig;
use crate::db::{chat_history, chat_llm_tools, chat_summaries};
use crate::mcp::McpManager;
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
    pub session_id:            i64,
    pub mcp:                   Arc<McpManager>,
    pub datetime_config:       DatetimeConfig,
    pub max_history_messages:  usize,
    pub max_tool_result_chars: Option<usize>,
    pub compactor:             Option<Arc<ContextCompactor>>,
    /// Effective working directory for this session. When set (e.g. from a project
    /// RunContext), it overrides the process cwd in the date/time/OS/WD tail block.
    pub working_directory:     Option<std::path::PathBuf>,
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
        // Reuses the memory-path resolution so the shown path is relative when the
        // index is under the session WD, absolute otherwise (it lives under Skald's
        // own cwd, so it shows as absolute inside project sessions). Skipped silently
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

        for (idx, entry) in history.iter().enumerate() {
            let is_previous_turn = current_turn_boundary.map_or(false, |b| idx < b);

            match entry.role {
                chat_history::Role::User | chat_history::Role::Agent => {
                    // Render attachments (if any) as a textual block appended to the
                    // user turn, generated on the fly — never persisted as content.
                    let content = match &entry.metadata {
                        Some(meta) if !meta.attachments.is_empty() => format!(
                            "{}{}",
                            entry.content,
                            core_api::message_meta::attachments_block(&meta.attachments),
                        ),
                        _ => entry.content.clone(),
                    };
                    // Coalesce consecutive user/agent rows into a single `role:user`
                    // turn. The DB keeps each message as its own row (distinct bubbles,
                    // per-message attachments), but the model must see one clean user
                    // turn — e.g. when several messages were injected back-to-back at a
                    // round boundary, or queued together while idle. `for_stack` already
                    // excludes `failed` rows, so only non-failed messages merge here.
                    match out.last_mut() {
                        Some(last) if last["role"] == "user" => {
                            let prev = last["content"].as_str().unwrap_or("").to_string();
                            last["content"] = Value::String(format!("{prev}\n\n{content}"));
                        }
                        _ => out.push(json!({ "role": "user", "content": content })),
                    }
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

                let cwd = self.working_directory.clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
                    .display()
                    .to_string();
                Some(format!(
                    "{date_line}\nOperating system: {}\nWorking directory: {cwd}\n\
                     Filesystem tools and execute_cmd use this working directory for relative paths — \
                     no need to `cd` into it first.",
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
    /// `$WD` expands to the session's effective working directory (RunContext WD, or the
    /// process cwd when unset). The shown path is **relative to that working directory
    /// when the file lives under it, absolute otherwise** — so when the agent references
    /// it back via `edit_file`/`write_file`, the loop's working-directory injection
    /// (which rewrites relative paths against the WD) resolves to the very same file.
    /// Loads an `inject_memory` entry, returning `(content, display_path)`.
    ///
    /// Virtual memory paths are read from SQLite: `user-memory/…` from the owner
    /// `pool`, `shared-memory/…` from the `shared_pool` (`system.db`). Everything
    /// else (`data/…`, `$WD/…`) is an ordinary disk read. A missing note / file
    /// yields `None`, rendered as "(file not created yet)".
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
        let wd = self.working_directory.clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let expanded = mem_path.replace("$WD", &wd.display().to_string());
        let abs = crate::tools::fs::resolve(&expanded)
            .unwrap_or_else(|_| std::path::PathBuf::from(&expanded));
        let display = match abs.strip_prefix(&wd) {
            Ok(rel) => rel.to_string_lossy().into_owned(),
            Err(_)  => abs.to_string_lossy().into_owned(),
        };
        (abs, display)
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
