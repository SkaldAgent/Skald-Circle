//! `SkaldDigest` — how an over-long tool result is condensed
//! (`agent_loop::projection::ToolResultDigest`).
//!
//! The crate decides *when* a result is too long (its `ResultLimit` gate, which
//! only shrinks turns the agent has already moved past); this decides *what to
//! say instead*, and that needs to know what each tool does — so it lives here,
//! next to the tools, not in the library.
//!
//! The replacement is always one informative line: the model must be able to
//! tell that a call succeeded and on what, without re-reading its output.

use agent_loop::projection::ToolResultDigest;
use serde_json::Value;

use crate::session::handler::preview_truncate;
use crate::tools::tool_names as tn;

pub struct SkaldDigest;

#[agent_loop::async_trait]
impl ToolResultDigest for SkaldDigest {
    async fn condense(&self, name: &str, args: &Value, result: &str) -> Option<String> {
        Some(summarize_tool_result(name, args, result))
    }
}

/// An informative 1-line summary of a tool call result.
pub fn summarize_tool_result(tool_name: &str, arguments: &Value, result: &str) -> String {
    let args = arguments;

    let char_count = result.len();
    let line_count = if result.trim().is_empty() { 0 } else { result.lines().count() };

    fn arg_str<'a>(args: &'a Value, key: &str) -> &'a str {
        args[key].as_str().unwrap_or("?")
    }

    match tool_name {
        tn::EXECUTE_CMD => {
            let cmd = args["command"].as_str().unwrap_or("");
            let cmd_display = preview_truncate(cmd, 77);
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
            let count = serde_json::from_str::<Vec<Value>>(result)
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
                    let sv = preview_truncate(v.as_str().unwrap_or_default(), 40);
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
    use serde_json::json;

    #[test]
    fn execute_cmd_reports_the_command_exit_code_and_size() {
        let s = summarize_tool_result(
            tn::EXECUTE_CMD,
            &json!({ "command": "ls -la /tmp" }),
            "exit: 0\nfile a\nfile b",
        );
        assert_eq!(s, "[execute_cmd] ran `ls -la /tmp` → exit 0, 3 lines output");
    }

    #[test]
    fn file_tools_report_the_path() {
        assert_eq!(
            summarize_tool_result("read_file", &json!({ "path": "notes.md" }), "0123456789"),
            "[read_file] read notes.md (10 chars)"
        );
        assert_eq!(
            summarize_tool_result("write_file", &json!({ "path": "a.txt" }), "ok"),
            "[write_file] wrote to a.txt"
        );
        // A missing argument degrades, never panics.
        assert_eq!(
            summarize_tool_result("edit_file", &json!({}), "ok"),
            "[edit_file] edited ?"
        );
    }

    #[test]
    fn sub_agent_and_activation_calls_name_their_target() {
        assert_eq!(
            summarize_tool_result(tn::EXECUTE_TASK, &json!({ "agent_id": "researcher" }), "abc"),
            "[execute_task] → researcher (3 chars result)"
        );
        assert_eq!(
            summarize_tool_result(tn::ACTIVATE_TOOLS, &json!({ "groups": ["gmail", "config"] }), ""),
            "[activate_tools] loaded: gmail, config"
        );
    }

    #[test]
    fn unknown_tools_fall_back_to_the_first_argument() {
        assert_eq!(
            summarize_tool_result("mcp__gmail__send", &json!({ "to": "x@y.z" }), "sent"),
            "[mcp__gmail__send] (4 chars result)"
        );
        assert_eq!(
            summarize_tool_result("weird_tool", &json!({ "q": "hello" }), "res"),
            "[weird_tool] q=hello (3 chars result)"
        );
        assert_eq!(
            summarize_tool_result("weird_tool", &json!({}), "res"),
            "[weird_tool] (3 chars result)"
        );
    }
}
