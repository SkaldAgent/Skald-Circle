pub const EXECUTE_TASK:           &str = "execute_task";
pub const EXECUTE_SUBTASK:        &str = "execute_subtask";
pub const UPDATE_SCRATCHPAD:      &str = "update_scratchpad";
pub const WRITE_TODOS:            &str = "write_todos";
pub const ASK_USER_CLARIFICATION: &str = "ask_user_clarification";
pub const ACTIVATE_TOOLS:         &str = "activate_tools";
/// Reserved `activate_tools` group name that loads all built-in `Config`-category
/// tools (system configuration) instead of an MCP server's tools.
pub const CONFIG_GROUP:           &str = "config";
pub const NOTIFY:                 &str = "notify";
pub const READ_NOTIFICATION:      &str = "read_notification";
pub const EXECUTE_CMD:            &str = "execute_cmd";
pub const SHOW_FILE_TO_USER:      &str = "show_file_to_user";
pub const IMAGE_GENERATE:         &str = "image_generate";
/// The one write verb of the read-only skills trees (blueprint §7.3). Named here
/// because the approval gate builds it a review card of its own.
pub const SKILL_REGISTER:         &str = "skill_register";
pub const SKILL_DELETE:           &str = "skill_delete";
/// Downloads a subtree of a public git repository into the caller's workspace
/// (blueprint §7.5). Deliberately not `git_clone`: it is shallow, drops `.git`,
/// sanitizes, and leaves a `.source.json` provenance ticket — none of which a
/// name borrowed from git would promise.
pub const FETCH_REPO:             &str = "fetch_repo";
