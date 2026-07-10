pub mod tickets;

use std::sync::Arc;

use anyhow::Result;
use sqlx::SqlitePool;

use crate::core::db::projects::{self, Project};
use crate::core::run_context::RunContext;

pub struct ProjectManager {
    db: Arc<SqlitePool>,
}

impl ProjectManager {
    pub fn new(db: Arc<SqlitePool>) -> Self {
        Self { db }
    }

    pub async fn list(&self) -> Result<Vec<Project>> {
        projects::list(&self.db).await
    }

    pub async fn get(&self, id: i64) -> Result<Option<Project>> {
        projects::get(&self.db, id).await
    }

    pub async fn create(
        &self,
        name:        &str,
        path:        &str,
        description: &str,
        run_context: Option<&RunContext>,
    ) -> Result<Project> {
        let rc_json = run_context.map(|rc| rc.to_db());
        projects::create(&self.db, name, path, description, rc_json.as_deref()).await
    }

    pub async fn update(
        &self,
        id:          i64,
        name:        &str,
        path:        &str,
        description: &str,
        run_context: Option<&RunContext>,
    ) -> Result<bool> {
        let rc_json = run_context.map(|rc| rc.to_db());
        projects::update(&self.db, id, name, path, description, rc_json.as_deref()).await
    }

    pub async fn delete(&self, id: i64) -> Result<bool> {
        projects::delete(&self.db, id).await
    }
}

/// Builds the runtime `RunContext` for working on `project`, layering project-runtime
/// fields over an optional pre-resolved `base` RC (which carries static config set at
/// creation time, e.g. `security_group`).
///
/// Runtime fields computed here:
/// - `working_directory` — always set to `project.path`.
/// - `allow_fs_writes`   — project tree + Skald's own `data/` directory.
/// - `system_prompt`     — project-context fragments prepended before any stored ones.
///
/// Shared by `ProjectTicketManager::start` (background ticket jobs) and the interactive
/// project-chat session provisioning, so both work with identical context.
pub fn build_runtime_run_context(project: &Project, base: Option<RunContext>) -> RunContext {
    let mut rc = base.unwrap_or_default();

    // Working directory is always the project path, overwritten at build time.
    rc.working_directory = Some(project.path.clone());

    // Absolute path to Skald's own data directory (user personal data store).
    let skald_data = std::env::current_dir()
        .unwrap_or_default()
        .join("data")
        .to_string_lossy()
        .into_owned();

    // Grant write access to the project tree and Skald's data directory.
    if !rc.allow_fs_writes.contains(&project.path) {
        rc.allow_fs_writes.push(project.path.clone());
    }
    if !rc.allow_fs_writes.contains(&skald_data) {
        rc.allow_fs_writes.push(skald_data.clone());
    }

    // Build runtime context fragments and prepend before any stored ones.
    // Note: working directory is intentionally omitted here — the date/time/OS/WD
    // tail block in MessageBuilder already reflects the effective WD from RunContext.
    let project_header = if project.description.is_empty() {
        format!("You are working on project \"{}\".", project.name)
    } else {
        format!("You are working on project \"{}\". Description: {}", project.name, project.description)
    };
    let mut injected = vec![
        project_header,
        format!(
            "Personal user data is available at: {}. \
             Consult it when the task requires knowledge about the user.",
            skald_data
        ),
    ];
    injected.extend(std::mem::take(&mut rc.system_prompt));
    rc.system_prompt = injected;

    rc
}
