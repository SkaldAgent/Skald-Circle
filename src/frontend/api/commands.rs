use std::sync::Arc;

use axum::{Json, extract::State};

use core_api::command::{CommandApi, CommandInfo};

use skald_core::skald::Skald;

/// `GET /api/commands` — list enabled custom slash commands (name + description)
/// for the composer autocomplete and the dynamic `/help`. Read-only: commands are
/// created by adding files under `commands/<name>/` (like agents), not via the API.
pub async fn list(State(skald): State<Arc<Skald>>) -> Json<Vec<CommandInfo>> {
    Json(skald.command_manager().list_enabled())
}
