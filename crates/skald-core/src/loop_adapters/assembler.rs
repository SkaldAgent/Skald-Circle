//! `SkaldAssembler` — Skald's history projection behind the crate's
//! `ContextAssembler` (port of `MessageBuilder::build`'s message-array half,
//! blueprint §10). Byte-parity with the current builder is the contract:
//! same layers, same tool-result texts, same DTL injections, same media rules.
//!
//! During phase 2 the old `MessageBuilder` still serves the legacy paths
//! (resume/recovery); the two are deleted together in phase 5.

use std::sync::Arc;

use agent_loop::activation::{ActivationSource, ToolRendering};
use agent_loop::context::{AssembleInput, ContextAssembler};
use agent_loop::store::{CallState, HistoryStore, Role};
use core_api::message_meta::{MessageMetadata, attachments_block};
use core_api::tool::MediaRef;
use core_api::user_fs::UserFs;
use serde_json::{Value, json};

use crate::compactor::SUMMARY_PREFIX;
use crate::config::DatetimeConfig;
use crate::loop_adapters::activation::SkaldActivationSource;
use crate::session::handler::media;
use crate::tools::tool_names as tn;

/// Stand-in for a tool-call turn's `reasoning_content` when none was recorded
/// (DeepSeek's thinking mode 400s on replay without it).
const REASONING_ROUNDTRIP_PLACEHOLDER: &str = "(no reasoning recorded for this step)";

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

/// Skald's `ContextAssembler`: static system → scratchpad → summary → history
/// (with DTL + media) → dynamic tail (+datetime) → tail reminder.
pub struct SkaldAssembler {
    /// Owner pool — scratchpad reads (keyed on `scratchpad_sid`).
    pub pool:                  Arc<sqlx::SqlitePool>,
    /// Scratchpad scope (session_id, or the parent's for async sub-tasks).
    pub scratchpad_sid:        i64,
    pub datetime_config:       DatetimeConfig,
    pub max_history_messages:  usize,
    pub max_tool_result_chars: Option<usize>,
    /// The history window applies only when compaction is disabled.
    pub compactor_enabled:     bool,
    /// The caller's fs view — media containment for inlining. `None` skips
    /// media inlining entirely.
    pub fs:                    Option<Arc<UserFs>>,
    /// DTL activations (consulted only in non-Inline modes).
    pub activation:            Option<SkaldActivationSource>,
}

#[agent_loop::async_trait]
impl ContextAssembler for SkaldAssembler {
    async fn build(
        &self,
        store: &Arc<dyn HistoryStore>,
        input: &AssembleInput,
    ) -> agent_loop::Result<Vec<Value>> {
        let mut out: Vec<Value> = Vec::new();

        // ── 1. Static system message ──────────────────────────────────────────
        let static_msg = if input.model.prompt_cache {
            json!({
                "role": "system",
                "content": [{ "type": "text", "text": input.system.base, "cache_control": { "type": "ephemeral" } }]
            })
        } else {
            json!({ "role": "system", "content": input.system.base })
        };
        out.push(static_msg);

        // ── 2. Scratchpad system message (before conversation) ────────────────
        let scratch = crate::db::scratchpad::for_session(&self.pool, self.scratchpad_sid).await?;
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

        // ── 3. Compaction summary + surviving history ─────────────────────────
        let summary = store.latest_summary(input.frame).await?;
        if let Some(s) = &summary {
            out.push(json!({
                "role": "system",
                "content": format!(
                    "{SUMMARY_PREFIX}\n\n{}\n\n\
                     [End of context summary — the following messages are the most recent exchanges in full.]",
                    s.text
                )
            }));
        }
        let mut history = match &summary {
            Some(s) => store.load_since(input.frame, s.covered_up_to).await?,
            None    => store.load(input.frame).await?,
        };

        if !self.compactor_enabled && history.len() > self.max_history_messages {
            history.drain(..history.len() - self.max_history_messages);
            if matches!(history.first().map(|m| m.role), Some(Role::Assistant)) {
                history.drain(..1);
            }
        }

        let current_turn_boundary = history
            .iter()
            .rposition(|e| matches!(e.role, Role::User | Role::Agent));

        // Inline-media turn group: trailing assistant rows are the in-flight
        // turn's own rounds; the current turn's user messages sit just before
        // them. Older-turn media degrades to the textual path block.
        let mut media_turn_start = history.len();
        while media_turn_start > 0 && matches!(history[media_turn_start - 1].role, Role::Assistant) {
            media_turn_start -= 1;
        }
        while media_turn_start > 0
            && matches!(history[media_turn_start - 1].role, Role::User | Role::Agent)
        {
            media_turn_start -= 1;
        }

        // DTL: tools activated at each assistant message (empty in Inline mode).
        let activation_defs: std::collections::HashMap<i64, Vec<Value>> =
            match (&self.activation, input.model.tool_rendering) {
                (Some(src), ToolRendering::Inline) => {
                    let _ = src;
                    Default::default()
                }
                (Some(src), _) => src
                    .activations(input.frame)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|a| (a.anchor.get(), a.defs))
                    .collect(),
                (None, _) => Default::default(),
            };

        // ── 4. Conversation history ───────────────────────────────────────────
        for (idx, entry) in history.iter().enumerate() {
            let is_previous_turn = current_turn_boundary.is_some_and(|b| idx < b);

            match entry.role {
                Role::System => {}
                Role::User | Role::Agent => {
                    let metadata: Option<MessageMetadata> = entry
                        .metadata
                        .as_ref()
                        .and_then(|v| serde_json::from_value(v.clone()).ok());
                    let (text, media_parts) = match &metadata {
                        Some(meta)
                            if !meta.attachments.is_empty()
                                && idx >= media_turn_start
                                && self.fs.is_some() =>
                        {
                            let fs = self.fs.as_deref().expect("guarded by is_some()");
                            let partition = media::partition(&meta.attachments, &input.model.capabilities, fs).await;
                            (
                                format!("{}{}", entry.content, attachments_block(&partition.rest)),
                                partition.parts,
                            )
                        }
                        Some(meta) if !meta.attachments.is_empty() => (
                            format!("{}{}", entry.content, attachments_block(&meta.attachments)),
                            Vec::new(),
                        ),
                        _ => (entry.content.clone(), Vec::new()),
                    };
                    push_user_chunk(&mut out, text, media_parts);
                }
                Role::Assistant => {
                    if entry.calls.is_empty() {
                        let mut msg = json!({ "role": "assistant", "content": entry.content });
                        if let Some(rc) = entry.reasoning.as_deref().filter(|s| !s.is_empty()) {
                            msg["reasoning_content"] = rc.into();
                            msg["reasoning"]         = rc.into();
                        }
                        out.push(msg);
                    } else {
                        let tc_array: Vec<Value> = entry.calls
                            .iter()
                            .map(|tc| json!({
                                "id":   tc.provider_id,
                                "type": "function",
                                "function": {
                                    "name":      tc.name,
                                    "arguments": serde_json::to_string(&tc.arguments)
                                        .unwrap_or_else(|_| "{}".into()),
                                }
                            }))
                            .collect();

                        let mut msg = json!({
                            "role":       "assistant",
                            "content":    entry.content,
                            "tool_calls": tc_array,
                        });
                        // DeepSeek thinking mode: a tool-calling assistant turn must
                        // carry a NON-EMPTY reasoning_content on replay.
                        let rc = entry.reasoning.as_deref()
                            .filter(|s| !s.is_empty())
                            .unwrap_or(REASONING_ROUNDTRIP_PLACEHOLDER);
                        msg["reasoning_content"] = rc.into();
                        msg["reasoning"]         = rc.into();
                        out.push(msg);

                        for tc in &entry.calls {
                            let result_content = match tc.state {
                                CallState::Done   => tc.result.clone().unwrap_or_default(),
                                CallState::Failed => format!(
                                    "Error: {}",
                                    tc.result.as_deref().unwrap_or("unknown error")
                                ),
                                CallState::Rejected => tc.result.clone()
                                    .unwrap_or_else(|| "User rejected this tool call.".to_string()),
                                CallState::Cancelled => tc.result.clone()
                                    .unwrap_or_else(|| "Tool call was cancelled by the user.".to_string()),
                                // 'pending'/'running' left behind by a crash or a lost
                                // connection: the call really was interrupted mid-flight.
                                _ => "Error: tool call was interrupted (connection lost before user approval). Please retry the operation.".to_string(),
                            };

                            let result_content = self.maybe_hide_tool_result(
                                result_content,
                                is_previous_turn,
                                &tc.name,
                                &tc.arguments,
                            );

                            let mut tool_msg = json!({
                                "role":         "tool",
                                "tool_call_id": tc.provider_id,
                                "content":      result_content,
                            });
                            // Anthropic DTL: an `activate_tools` result becomes a set of
                            // `tool_reference`s.
                            if matches!(input.model.tool_rendering, ToolRendering::DeferredToolReference)
                                && tc.name == tn::ACTIVATE_TOOLS
                                && let Some(adefs) = activation_defs.get(&entry.id.get())
                            {
                                let names: Vec<Value> = adefs.iter()
                                    .filter_map(|d| d["function"]["name"].as_str())
                                    .map(|n| Value::String(n.to_string()))
                                    .collect();
                                if !names.is_empty() {
                                    tool_msg["_tool_references"] = Value::Array(names);
                                }
                            }
                            out.push(tool_msg);
                        }

                        // Tool-produced media of the current turn: inline as a
                        // synthetic `user` message right after the tool-result group.
                        if idx >= media_turn_start
                            && let Some(fs) = self.fs.as_deref()
                        {
                            let mut refs: Vec<MediaRef> = Vec::new();
                            for tc in &entry.calls {
                                if let Some(mj) = tc.extras["media"].as_str()
                                    && let Ok(mut v) = serde_json::from_str::<Vec<MediaRef>>(mj)
                                {
                                    refs.append(&mut v);
                                }
                            }
                            if !refs.is_empty() {
                                let parts = media::inline_paths(&refs, &input.model.capabilities, fs).await;
                                if !parts.is_empty() {
                                    out.push(json!({ "role": "user", "content": parts }));
                                }
                            }
                        }

                        // Kimi K3 DTL: the tools activated at this assistant message,
                        // as a `system` message carrying a `tools` field, right after
                        // its tool-result group (append-only → cache-safe).
                        if matches!(input.model.tool_rendering, ToolRendering::SystemToolBlock)
                            && let Some(adefs) = activation_defs.get(&entry.id.get())
                            && !adefs.is_empty()
                        {
                            out.push(json!({ "role": "system", "tools": adefs }));
                        }
                    }
                }
            }
        }

        // ── 5. Dynamic tail (extra dynamic + datetime) ────────────────────────
        {
            let datetime_line = self.datetime_line();
            let extra_dynamic = input.system.dynamic_tail.first().map(String::as_str);
            let tail = match (extra_dynamic, datetime_line.as_deref()) {
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
        if let Some(reminder) = &input.system.tail_reminder {
            out.push(json!({ "role": "system", "content": reminder }));
        }

        Ok(out)
    }
}

impl SkaldAssembler {
    /// The current date/time + OS + cwd block (empty when disabled).
    fn datetime_line(&self) -> Option<String> {
        if !self.datetime_config.enabled {
            return None;
        }
        let now_utc = chrono::Utc::now();
        let secs = now_utc.timestamp();

        let secs = match self.datetime_config.round_minutes {
            Some(m) if m > 0 => {
                let bucket = (m as i64) * 60;
                (secs / bucket) * bucket
            }
            _ => secs,
        };

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
    }

    /// Replaces an over-limit previous-turn result with an informative 1-liner.
    fn maybe_hide_tool_result(
        &self,
        result:           String,
        is_previous_turn: bool,
        tool_name:        &str,
        arguments:        &Value,
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
}

// ── Free helpers (ported verbatim from message_builder.rs) ─────────────────────

/// Appends one user/agent chunk, coalescing with a preceding `user` message.
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
fn summarize_tool_result(tool_name: &str, arguments: &Value, result: &str) -> String {
    let args = arguments;

    let char_count = result.len();
    let line_count = if result.trim().is_empty() { 0 } else { result.lines().count() };

    fn arg_str<'a>(args: &'a serde_json::Value, key: &str) -> &'a str {
        args[key].as_str().unwrap_or("?")
    }

    match tool_name {
        tn::EXECUTE_CMD => {
            let cmd = args["command"].as_str().unwrap_or("");
            let cmd_display = crate::session::handler::preview_truncate(cmd, 77);
            let exit_code = result
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("exit: "))
                .unwrap_or("?");
            format!("[execute_cmd] ran `{cmd_display}` → exit {exit_code}, {line_count} lines output")
        }

        "read_file" | "read_file_chunk" => {
            let path = arg_str(args, "path");
            format!("[{tool_name}] read {path} ({char_count} chars)")
        }

        "write_file" => {
            let path = arg_str(args, "path");
            format!("[write_file] wrote to {path}")
        }

        "edit_file" | "patch_file" => {
            let path = arg_str(args, "path");
            format!("[{tool_name}] edited {path}")
        }

        "list_dir" | "glob" => {
            let path = args["path"].as_str()
                .or_else(|| args["pattern"].as_str())
                .unwrap_or("?");
            format!("[{tool_name}] {path} ({char_count} chars)")
        }

        "list_items" => {
            let kind = arg_str(args, "type");
            format!("[list_items] {kind} ({char_count} chars)")
        }

        "toggle_item" => {
            let kind    = arg_str(args, "kind");
            let id      = arg_str(args, "id");
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
            let agent = arg_str(args, "agent_id");
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
                    let sv = crate::session::handler::preview_truncate(v.as_str().unwrap_or_default(), 40);
                    format!(" {k}={sv}")
                })
                .unwrap_or_default();
            format!("[{tool_name}]{first_arg} ({char_count} chars result)")
        }
    }
}
