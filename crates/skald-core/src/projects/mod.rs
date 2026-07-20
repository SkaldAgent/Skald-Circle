use crate::db::projects::Project;
use crate::run_context::RunContext;

/// Builds the runtime `RunContext` for working on `project`, layering the project's
/// working directory + a context header over an optional pre-resolved `base` RC (which
/// carries static config set at creation time, e.g. `security_group`).
///
/// `working_directory` is the **agent path** `projects/{owner_username}/{slug}` — the
/// same namespace the fs-tools and `execute_cmd` route through (the host/container
/// mapping is handled by `UserFs`). Writes there are auto-allowed by the seeded
/// `projects/*` approval rule and physically gated by the per-member read-only mount,
/// so no host-path `allow_fs_writes` grant is needed (that was the old single-user
/// model, which predated per-user containers).
pub fn build_runtime_run_context(
    project:        &Project,
    owner_username: &str,
    base:           Option<RunContext>,
) -> RunContext {
    let mut rc = base.unwrap_or_default();

    rc.working_directory = Some(format!("projects/{owner_username}/{}", project.slug));

    let project_header = if project.description.is_empty() {
        format!("You are working on project \"{}\".", project.name)
    } else {
        format!(
            "You are working on project \"{}\". Description: {}",
            project.name, project.description
        )
    };
    let mut injected = vec![project_header];
    injected.extend(std::mem::take(&mut rc.system_prompt));
    rc.system_prompt = injected;

    rc
}
