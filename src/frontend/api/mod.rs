pub mod agents;
pub mod auth;
pub mod commands;
pub mod config;
pub mod approval;
pub mod caps;
pub mod cron;
pub mod dev;
pub mod file_watch;
pub mod guard;
pub mod stats;
pub mod files;
pub mod image_generate_models;
pub mod images;
pub mod inbox;
pub mod llm;
pub mod marketplace;
pub mod mcp;
pub mod mcp_media;
pub mod plugins;
pub mod projects;
pub mod roles;
pub mod run_context;
pub mod sessions;
pub mod setup;
pub mod shared_folders;
pub mod transcribe_audio;
pub mod transcribe_models;
pub mod tts_models;
pub mod uploads;
pub mod users_mgmt;
pub mod ws;
pub mod ws_session;

use std::sync::Arc;

use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
};

use skald_core::skald::Skald;

pub fn router() -> Router<Arc<Skald>> {
    Router::new()
        .route("/agents",                       get(agents::list))
        .route("/agents/{id}",                  get(agents::get))
        .route("/agents/{id}/icon",             get(agents::icon))
        // Custom slash commands (file-based, read-only listing for autocomplete + /help)
        .route("/commands",                     get(commands::list))
        .route("/sessions",                             get(sessions::list_sessions).post(sessions::create))
        // First-run setup
        .route("/setup/status",                          get(setup::status))
        .route("/setup/profiles",                        get(setup::profiles))
        .route("/setup/user",                            post(setup::create_user))
        // Auth
        .route("/auth/login",                            post(auth::login))
        .route("/auth/me",                               get(auth::me))
        .route("/auth/logout",                           post(auth::logout))
        .route("/auth/profile",                          put(auth::update_profile))
        .route("/auth/change-password",                  post(auth::change_password))
        .route("/sessions/{id}",                        get(sessions::get_session_detail))
        .route("/web/messages",                         get(sessions::web_messages))
        .route("/{source}/messages",                    get(sessions::source_messages))
        // File attachments: streamed to disk, so the default body-size limit is
        // disabled on this route only.
        .route("/{source}/uploads",                     post(uploads::upload).layer(DefaultBodyLimit::disable()))
        // Full execution detail for one tool call (the tool-detail page).
        .route("/tools/{tool_call_id}",                 get(sessions::tool_detail))
        // Source-agnostic approval resolve, keyed by globally-unique tool_call_id.
        .route("/tools/{tool_call_id}/resolve",         post(sessions::resolve_tool))
        // Back-compat alias for older web clients that POST to /web/tools/...
        .route("/web/tools/{tool_call_id}/resolve",     post(sessions::resolve_tool))
        .route("/ws",                                   get(ws::handler))
        .route("/ws/session/{id}",                      get(ws_session::handler))
        .route("/file/watch",                           get(file_watch::handler))
        // LLM selector (for copilot dropdown)
        .route("/llm/models/selector",          get(llm::selector))
        // LLM providers
        .route("/llm/providers/types",          get(llm::provider_types))
        .route("/llm/providers",                get(llm::list_providers).post(llm::create_provider))
        .route("/llm/providers/{id}",           get(llm::get_provider).put(llm::update_provider).delete(llm::delete_provider))
        .route("/llm/providers/{id}/models",    get(llm::provider_models))
        .route("/llm/providers/{id}/reasoning-mode", get(llm::provider_reasoning_mode))
        // LLM models
        .route("/llm/models",                   get(llm::list_models).post(llm::create_model))
        .route("/llm/models/{id}",              get(llm::get_model).put(llm::update_model).delete(llm::delete_model))
        // Transcription — audio upload + model CRUD
        .route("/transcribe/audio",                    post(transcribe_audio::transcribe_audio))
        .route("/transcribe/has",                      get(transcribe_audio::has_transcribe))
        .route("/transcribe/models",                   get(transcribe_models::list_models).post(transcribe_models::create_model))
        .route("/transcribe/models/{id}",              get(transcribe_models::get_model).put(transcribe_models::update_model).delete(transcribe_models::delete_model))
        .route("/transcribe/providers/{id}/models",    get(transcribe_models::provider_models))
        // Image generation models
        .route("/image-generate/models",        get(image_generate_models::list_models).post(image_generate_models::create_model))
        .route("/image-generate/models/{id}",   get(image_generate_models::get_model).put(image_generate_models::update_model).delete(image_generate_models::delete_model))
        // TTS models
        .route("/tts/models",                   get(tts_models::list_models).post(tts_models::create_model))
        .route("/tts/models/{id}",              get(tts_models::get_model).put(tts_models::update_model).delete(tts_models::delete_model))
        .route("/tts/providers/{id}/models",    get(tts_models::provider_models))
        // Projects
        .route("/projects",                              get(projects::list).post(projects::create))
        .route("/projects/{id}",                         get(projects::get_project).put(projects::update).delete(projects::delete))
        .route("/projects/{id}/members",                 post(projects::add_member))
        .route("/projects/{id}/members/{user_id}",       delete(projects::remove_member))
        .route("/projects/{id}/session",                 post(projects::open_session))
        // Cron jobs
        .route("/cron/jobs",                    get(cron::list))
        .route("/cron/jobs/{id}",               delete(cron::delete_job))
        .route("/cron/jobs/{id}/kill",          post(cron::kill_job))
        .route("/cron/jobs/{id}/toggle",        post(cron::toggle))
        .route("/cron/jobs/{id}/run-context",   patch(cron::set_run_context))
        .route("/cron/runs",                    get(cron::list_runs))
        // Agent Inbox — unified pending approvals + clarifications
        .route("/inbox",                                          get(inbox::list))
        .route("/inbox/approvals/{request_id}/resolve",           post(inbox::resolve_approval))
        .route("/inbox/clarifications/{request_id}/resolve",      post(inbox::resolve_clarification))
        .route("/inbox/elicitations/{request_id}/resolve",        post(inbox::resolve_elicitation))
        // Approval — pending list + cross-session resolve (kept for backwards compat)
        .route("/approval/pending",             get(approval::list_pending))
        .route("/approval/pending/{request_id}/resolve", post(approval::resolve_pending))
        // Approval rules
        .route("/approval/rules",               get(approval::list_rules).post(approval::create_rule))
        .route("/approval/rules/{id}",          put(approval::update_rule).delete(approval::delete_rule))
        .route("/approval/tools",               get(approval::list_tools))
        // Tool permission groups
        .route("/tool-permission-groups",                    get(run_context::list_groups).post(run_context::create_group))
        .route("/tool-permission-groups/{id}",               put(run_context::update_group).delete(run_context::delete_group))
        .route("/tool-permission-groups/{id}/duplicate",     post(run_context::duplicate_group))
        // The caller's own selectable security-groups (for the chat picker)
        .route("/my/security-groups",                        get(run_context::my_security_groups))
        // Session tool_group assignment (runtime)
        .route("/sessions/{session_id}/run-context", put(run_context::set_session_run_context))
        // MCP / Connectors (blueprint §14/§15)
        .route("/mcp/servers",                  get(mcp::list_servers))
        // admin: the remote marketplace feed (consultative — installing is the
        // admin's act, and it lands in the catalog below)
        .route("/mcp/marketplace",              get(marketplace::list))
        .route("/mcp/marketplace/install",      post(marketplace::install))
        .route("/mcp/marketplace/{id}/icon",    get(marketplace::icon))
        // admin: catalog + globally-active connectors
        .route("/mcp/catalog",                  get(mcp::catalog_list).post(mcp::catalog_upsert))
        .route("/mcp/catalog/{id}",             delete(mcp::catalog_delete))
        // The icon of an installed connector, off the local `connectors/` folder.
        // Any logged-in user, not just a catalog manager — see `catalog_icon`.
        .route("/mcp/catalog/{name}/icon",      get(mcp::catalog_icon))
        .route("/mcp/global",                   get(mcp::global_list).post(mcp::global_enable))
        .route("/mcp/global/{id}",              delete(mcp::global_delete))
        .route("/mcp/global/{id}/access",       get(mcp::global_get_access).put(mcp::global_set_access))
        // admin: OAuth providers (client credentials for per-user sign-in, §15)
        .route("/mcp/providers",                get(mcp::providers_list).post(mcp::providers_upsert))
        .route("/mcp/providers/{name}",         delete(mcp::providers_delete))
        // user: available catalog + per-user activation
        .route("/mcp/available",                get(mcp::available))
        .route("/mcp/activate",                 post(mcp::activate))
        .route("/mcp/test",                     post(mcp::test))
        .route("/mcp/activated",                get(mcp::activated_list))
        .route("/mcp/activated/{id}",           delete(mcp::deactivate))
        // user: interactive OAuth login for a pending per-user connector (§15)
        .route("/mcp/oauth/start",              post(mcp::oauth_start))
        .route("/mcp/oauth/complete",           post(mcp::oauth_complete))
        // user: interactive QR / device login for a pending per-user connector (§15)
        .route("/mcp/login/status",             post(mcp::login_status))
        .route("/mcp/login/reset",              post(mcp::login_reset))
        // Dev / debug
        .route("/dev/debug_mode",               get(dev::get_debug_mode).post(dev::set_debug_mode).put(dev::set_debug_mode))
        .route("/dev/llm-requests",             get(dev::list_llm_requests))
        .route("/dev/llm-requests/{id}",        get(dev::get_llm_request))

        .route("/stats/llm",                    get(stats::llm_stats))
        // Config properties
        .route("/config",                       get(config::list_properties))
        .route("/config/{key}",                 put(config::set_property))
        // Plugins — admin: manage + access grants; user: own view + own config
        .route("/plugins",                      get(plugins::list))
        .route("/plugins/mine",                 get(plugins::mine))
        .route("/plugins/pages",                get(plugins::pages))
        .route("/plugins/{id}",                 put(plugins::update))
        .route("/plugins/{id}/access",          get(plugins::get_access).put(plugins::set_access))
        .route("/plugins/{id}/my-config",       put(plugins::update_my_config))
        // Roles
        .route("/roles",                        get(roles::list).post(roles::create))
        .route("/roles/{id}",                   put(roles::update).delete(roles::delete))
        // User management
        .route("/users",                        get(users_mgmt::list).post(users_mgmt::create))
        .route("/users/{id}",                   put(users_mgmt::update).delete(users_mgmt::delete))
        .route("/users/{id}/password",          post(users_mgmt::reset_password))
        // Per-user connector access (admin curates which registered MCP connectors
        // each user may use — globals + per-user catalog, in one surface).
        .route("/users/{id}/connectors",        get(mcp::user_connectors_get).put(mcp::user_connectors_set))

        // Shared on-disk folders (blueprint §6) — admin-curated, capability-gated.
        .route("/shared-folders",               get(shared_folders::list).post(shared_folders::create))
        .route("/shared-folders/{id}",          patch(shared_folders::update_description).delete(shared_folders::delete))
        .route("/shared-folders/{id}/members",  post(shared_folders::add_member))
        .route("/shared-folders/{id}/members/{user_id}", delete(shared_folders::remove_member))
        // Images (generated by image_generate tool)
        .route("/images/{task_id}",             get(images::get_image))
        // MCP tool-result media (images/audio/files returned by MCP servers)
        .route("/mcp-media/{file}",             get(mcp_media::get_media))
        // Files
        .route("/files",                        get(files::list_files))
        .route("/files/dir",                    get(files::list_dir))
        .route("/file",                         get(files::get_file))
        .route("/file",                         post(files::create_file))
        .route("/file/upload",                  post(files::upload_file)
                                                .layer(DefaultBodyLimit::max(files::MAX_UPLOAD_BYTES)))
        .route("/file",                         put(files::save_file))
        .route("/file",                         patch(files::rename_file))
        .route("/file",                         delete(files::delete_file))
}

pub struct ApiError {
    status:  StatusCode,
    message: String,
}

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: msg.into() }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: msg.into() }
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::UNAUTHORIZED, message: msg.into() }
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::FORBIDDEN, message: msg.into() }
    }

    pub fn payload_too_large(msg: impl Into<String>) -> Self {
        Self { status: StatusCode::PAYLOAD_TOO_LARGE, message: msg.into() }
    }
}

/// Resolves the authenticated caller's per-user runtime context, or `401` when the
/// user's database is locked (not logged in). Every owner-bound HTTP handler starts
/// here to route reads/writes to the caller's own `{userid}.db` instead of the
/// shared registry pool. The `AuthUser` is injected by [`guard::require_auth`].
pub async fn require_context(
    skald:   &Skald,
    user_id: &str,
) -> Result<Arc<skald_core::skald::UserContext>, ApiError> {
    skald.user_context(user_id).await
        .ok_or_else(|| ApiError::unauthorized("session expired — please log in again"))
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        let err = e.into();
        tracing::error!(error = ?err, "internal API error");
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, message: err.to_string() }
    }
}
