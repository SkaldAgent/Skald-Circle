use crate::db::projects::Project;
use crate::run_context::RunContext;

/// A project member's display info for the system-prompt block.
///
/// The display name is what the user usually goes by (fallback to the username);
/// the username is the unique handle. Both are shown so the agent can refer to a
/// member either way the user does in conversation.
pub struct ProjectMemberView {
    pub display_name: String,
    pub username:     String,
}

/// Builds the runtime `RunContext` for working on `project`, layering a project
/// context block over an optional pre-resolved `base` RC (which carries static
/// config set at creation time, e.g. `security_group`).
///
/// The session working directory is **always** the user's home (`~`); project
/// files are referenced by their absolute agent path `projects/{owner}/{slug}`,
/// which `UserFs` routes to the per-member bind mount. This keeps the working
/// directory stable across sessions (so MCP servers running in the container see
/// a consistent cwd) and avoids silent path rewriting inside tool calls.
///
/// Writes under `projects/*` are auto-allowed by the seeded approval rule and
/// physically gated by the per-member read-only mount, so no host-path
/// `allow_fs_writes` grant is needed.
pub fn build_project_run_context(
    project:        &Project,
    owner_username: &str,
    members:        &[ProjectMemberView],
    base:           Option<RunContext>,
) -> RunContext {
    let mut rc = base.unwrap_or_default();

    let project_path = format!("projects/{owner_username}/{}", project.slug);
    rc.project_root = Some(project_path.clone());

    let mut block = vec![
        format!("You are working on project \"{}\".", project.name),
        format!("Project folder: {project_path}"),
    ];
    if !project.description.is_empty() {
        block.insert(1, format!("Description: {}", project.description));
    }
    // Sharing line: list members other than the owner, or note the project is private.
    // The owner is implicit (they are the user the agent is talking to), so they are
    // excluded from the list. Display name first, username in parentheses.
    let others: Vec<String> = members
        .iter()
        .filter(|m| m.username != owner_username)
        .map(|m| {
            if m.display_name.is_empty() || m.display_name == m.username {
                m.username.clone()
            } else {
                format!("{} ({})", m.display_name, m.username)
            }
        })
        .collect();
    let sharing = if others.is_empty() {
        "Shared with: not shared with anyone yet.".to_string()
    } else {
        format!("Shared with: {}.", others.join(", "))
    };
    block.push(sharing);

    // Prepend the project block to any existing system_prompt fragments.
    let mut injected = block;
    injected.extend(std::mem::take(&mut rc.system_prompt));
    rc.system_prompt = injected;

    rc
}
