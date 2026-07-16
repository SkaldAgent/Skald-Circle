pub mod approval_rules;
pub mod project_tickets;
pub mod projects;
pub mod chat_history;
pub mod chat_llm_tools;
pub mod chat_sessions;
pub mod chat_sessions_stack;
pub mod chat_summaries;
pub mod config;
pub mod job_runs;
pub mod known_tools;
pub mod llm_requests;
pub mod llm_request_payloads;
pub mod mcp_catalog;
pub mod mcp_events;
pub mod mcp_global_access;
pub mod mcp_global_servers;
pub mod mcp_user_servers;
pub mod memory_docs;
pub mod plugins;
pub mod role_capabilities;
pub mod roles;
pub mod scheduled_jobs;
pub mod scratchpad;
pub mod session_mcp_grants;
pub mod shared_folders;
pub mod sources;
pub mod stack_mcp_grants;
pub mod tool_permission_groups;
pub mod users;

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::{SqlitePool, sqlite::{SqliteConnectOptions, SqliteJournalMode, SqliteSynchronous}};

use crate::crypto::Dek;

/// Every database file lives here, so backup, export and per-user erasure are a
/// matter of files rather than tables.
pub const DATABASE_DIR: &str = "database";

/// System database: instance-wide state, shared by every user. Fixed path — not
/// configurable.
pub const SYSTEM_DB_PATH: &str = "database/system.db";

/// `{dir}/{userid}.db`. Keyed by the opaque user id and never the username, so a
/// rename never has to touch the file. `dir` is a parameter rather than the
/// [`DATABASE_DIR`] constant so nothing depends on the process's working
/// directory — tests in particular.
pub fn user_db_path(dir: &Path, user_id: &str) -> PathBuf {
    dir.join(format!("{user_id}.db"))
}

/// The `-wal` and `-shm` sidecars SQLite keeps beside a database in WAL mode.
/// Erasing a user means erasing these too.
pub fn user_db_sidecars(path: &Path) -> [PathBuf; 2] {
    let ext = |suffix: &str| {
        let mut p = path.to_path_buf().into_os_string();
        p.push(suffix);
        PathBuf::from(p)
    };
    [ext("-wal"), ext("-shm")]
}

fn ensure_parent(path: &Path) -> Result<()> {
    // `create_if_missing` creates the file, never its parent directory.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create database directory {}", parent.display()))?;
    }
    Ok(())
}

fn tuned(opts: SqliteConnectOptions) -> SqliteConnectOptions {
    // WAL lets readers run alongside a single writer, and `busy_timeout` makes a
    // writer *wait* for the lock instead of failing immediately with SQLITE_BUSY
    // ("database is locked"). Without these, concurrent writers — e.g. the
    // mobile-connector persisting its E2E `send_counter` while the chat loop /
    // cron write history — abort mid-operation, which silently drops outbound
    // mobile messages (inbox_update never reaches the device).
    opts.journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
}

/// Connect options for one user's database.
///
/// When `key` is present the pool is a SQLCipher pool: sqlx puts `PRAGMA key`
/// ahead of every other pragma, so the page cipher is armed before `journal_mode`
/// touches the file. Without a key the same code opens an ordinary SQLite file —
/// which is exactly what a cleartext user gets.
///
/// The returned value carries the raw key. **Never** `{:?}` it: `SqliteConnectOptions`
/// prints its pragmas, so a debug format anywhere near this would write the DEK,
/// in hex, into the logs.
fn user_options(path: &Path, key: Option<&Dek>, create: bool) -> SqliteConnectOptions {
    let opts = SqliteConnectOptions::new().filename(path).create_if_missing(create);
    let opts = match key {
        Some(dek) => opts.pragma("key", dek.to_pragma()),
        None => opts,
    };
    tuned(opts)
}

/// Forces a page read so a wrong key fails here, at open time, rather than at the
/// first unrelated query. SQLCipher answers "file is not a database" when the key
/// does not decrypt the header.
async fn probe(pool: &SqlitePool) -> Result<()> {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sqlite_master")
        .fetch_one(pool)
        .await
        .context("database did not open — wrong key, or not a database")?;
    Ok(())
}

/// The system database: registry tables, plus — for now — the owner tables.
///
/// Owner tables live here **transitionally**. Nothing has been migrated to
/// per-user pools yet, so sessions, history and jobs still land in `system.db`.
/// When the call sites move to `UserManager::pool_of`, this second call goes
/// away and the owner-without-a-user gets a file of its own.
pub async fn init_system_pool(path: &str) -> Result<SqlitePool> {
    ensure_parent(Path::new(path))?;
    let opts = tuned(SqliteConnectOptions::from_str(path)?.create_if_missing(true));
    let pool = SqlitePool::connect_with(opts).await?;
    create_registry_tables(&pool).await?;
    create_owner_tables(&pool).await?;
    crate::boot::section("Database initialised".to_string());
    Ok(pool)
}

/// Provisions `database/{userid}.db` and lays down the owner schema.
///
/// Only this function may create a user's database. Login goes through
/// [`open_user_pool`], which refuses to create anything: a missing file there is
/// data loss, and creating a fresh empty one under the right password would hide
/// it instead of reporting it.
pub async fn create_user_pool(path: &Path, key: Option<&Dek>) -> Result<SqlitePool> {
    ensure_parent(path)?;
    let pool = SqlitePool::connect_with(user_options(path, key, true)).await?;
    probe(&pool).await?;
    create_owner_tables(&pool).await?;
    Ok(pool)
}

/// Opens an existing user database. Never creates one — see [`create_user_pool`].
pub async fn open_user_pool(path: &Path, key: Option<&Dek>) -> Result<SqlitePool> {
    let pool = SqlitePool::connect_with(user_options(path, key, false)).await?;
    probe(&pool).await?;
    Ok(pool)
}

// ── Registry tables ───────────────────────────────────────────────────────────
//
// Instance-wide, readable without any user key: the directory you must open
// before you know who exists. Nothing here is scoped to one user.

async fn create_registry_tables(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS llm_providers (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT    NOT NULL UNIQUE,
            type        TEXT    NOT NULL,
            api_key     TEXT,
            base_url    TEXT,
            description TEXT,
            removed_at  TEXT,
            created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        // `name` is the unique identity + resolution key (LlmManager keys its
        // in-memory model map by name). There is deliberately NO
        // UNIQUE(provider_id, model_id): the same underlying model may be
        // registered multiple times under one provider with different aliases
        // and reasoning settings (e.g. "glm-4.6" vs "glm-4.6-thinking").
        "CREATE TABLE IF NOT EXISTS llm_models (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id       INTEGER NOT NULL REFERENCES llm_providers(id) ON DELETE CASCADE,
            model_id          TEXT    NOT NULL,
            name              TEXT    NOT NULL UNIQUE,
            strength          TEXT,
            scope             TEXT    NOT NULL DEFAULT '[]',
            is_default        INTEGER NOT NULL DEFAULT 0,
            priority          INTEGER NOT NULL DEFAULT 100,
            extra_params      TEXT,
            removed_at        TEXT,
            context_length    INTEGER,
            max_output_tokens INTEGER,
            knowledge_cutoff  TEXT,
            capabilities      TEXT    NOT NULL DEFAULT '[]',
            reasoning         TEXT,
            created_at        TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS transcribe_models (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id INTEGER NOT NULL REFERENCES llm_providers(id),
            model_id    TEXT    NOT NULL,
            name        TEXT    NOT NULL UNIQUE,
            language    TEXT,
            priority    INTEGER NOT NULL DEFAULT 100,
            removed_at  TEXT,
            created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE(provider_id, model_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS image_generate_models (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id INTEGER NOT NULL REFERENCES llm_providers(id),
            model_id    TEXT    NOT NULL,
            name        TEXT    NOT NULL UNIQUE,
            priority    INTEGER NOT NULL DEFAULT 100,
            removed_at  TEXT,
            created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE(provider_id, model_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tts_models (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            provider_id  INTEGER NOT NULL REFERENCES llm_providers(id),
            model_id     TEXT    NOT NULL,
            voice_id     TEXT,
            name         TEXT    NOT NULL UNIQUE,
            description  TEXT,
            instructions TEXT,
            priority     INTEGER NOT NULL DEFAULT 100,
            removed_at   TEXT,
            response_format TEXT,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE(provider_id, model_id)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS plugins (
            id         TEXT    PRIMARY KEY,
            enabled    INTEGER NOT NULL DEFAULT 0,
            config     TEXT    NOT NULL DEFAULT '{}',
            created_at TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS tool_permission_groups (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL,
            description TEXT,
            created_at  TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS approval_rules (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id     TEXT,
            source       TEXT,
            tool_pattern TEXT    NOT NULL,
            action       TEXT    NOT NULL DEFAULT 'require'
                             CHECK(action IN ('require', 'allow', 'deny')),
            note         TEXT,
            priority     INTEGER NOT NULL DEFAULT 100,
            path_pattern TEXT,
            group_id     TEXT    REFERENCES tool_permission_groups(id),
            created_at   TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS config (
            key        TEXT PRIMARY KEY,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    // Every tool ever offered to the LLM, recorded by `ToolDiscovery` at
    // injection time. Lets the approval / Security-groups UI list and gate tools
    // that are injected dynamically outside the `ToolRegistry` (interface tools,
    // plugin tools, provider tools).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS known_tools (
            name        TEXT PRIMARY KEY,
            description TEXT NOT NULL DEFAULT '',
            schema      TEXT,
            first_seen  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
            last_seen   INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        )",
    )
    .execute(pool)
    .await?;

    // Spend/telemetry metadata. Stays in the registry so cost charts run without
    // decrypting anything: the admin sees how much, when and which model — never
    // what was said. `session_id` / `stack_id` are bare integers, not foreign
    // keys, precisely because the rows they point at live in another file.
    // `user_id` correlates the row with the payload in `{userid}.db`.
    // Payloads (request/response bodies, headers) live in `llm_request_payloads`
    // in the owner bucket — they are conversation content, behind the user key.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS llm_requests (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            request_id       TEXT,
            user_id          TEXT,
            session_id       INTEGER,
            stack_id         INTEGER,
            model_name       TEXT    NOT NULL,
            error_text       TEXT,
            input_tokens     INTEGER,
            output_tokens    INTEGER,
            cache_read_tokens     INTEGER,
            cache_creation_tokens INTEGER,
            duration_ms      INTEGER NOT NULL DEFAULT 0,
            created_at       TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_llm_requests_created
         ON llm_requests (created_at)",
    )
    .execute(pool)
    .await?;

    // User directory + auth material. Read before every login, so it lives in
    // the registry — which means it must never hold anything that derives a
    // user's key: `database_password` is the DEK sealed under a key derived
    // from the password, useless without it.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS roles (
            id               TEXT PRIMARY KEY,
            label            TEXT NOT NULL,
            permission_group TEXT NOT NULL,
            attrs            TEXT,
            created_at       TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    crate::db::roles::seed_admin(pool).await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS users (
            id                TEXT PRIMARY KEY,
            username          TEXT    NOT NULL UNIQUE,
            display_name      TEXT,
            role_id           TEXT    NOT NULL REFERENCES roles(id),
            encrypted         INTEGER NOT NULL,
            kdf_params        TEXT,
            kdf_salt          BLOB,
            database_password BLOB,
            password_hash     BLOB,
            active            INTEGER NOT NULL DEFAULT 1,
            created_at        TEXT    NOT NULL DEFAULT (datetime('now')),
            updated_at        TEXT    NOT NULL DEFAULT (datetime('now')),
            CHECK (
                (encrypted = 1 AND database_password IS NOT NULL AND password_hash IS NULL)
             OR (encrypted = 0 AND database_password IS NULL)
            )
        )",
    )
    .execute(pool)
    .await?;

    // Shared on-disk folders (blueprint §6/§0.1): a named directory
    // `{WD}/shared/{folder_name}` bind-mounted into the container of each member.
    // Registry tables — instance-wide config, readable without any user key. The
    // membership is a **junction table** (not a JSON array) so a member can be
    // read-only, and so the mount topology / fs routing can query it in both
    // directions. FK `user_id → users(id)` is registry→registry (same file):
    // allowed, unlike an owner→registry key.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS shared_folders (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            folder_name TEXT    NOT NULL UNIQUE,
            created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS shared_folder_members (
            folder_id INTEGER NOT NULL REFERENCES shared_folders(id) ON DELETE CASCADE,
            user_id   TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            can_write INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY (folder_id, user_id)
        )",
    )
    .execute(pool)
    .await?;

    // ── MCP catalog + globally-active instances (blueprint §7/§14/§15) ──────────
    //
    // Registry tables: instance-wide MCP config, listable without any user key so
    // the admin can render the "Connectors" catalog. The catalog is the admin's
    // vetted set of installable connectors; a user later *instantiates* a per-user
    // one into their own `{userid}.db` (`mcp_user_servers`, owner bucket) or the
    // admin *enables* a global one here. Per-user credentials never land here — the
    // catalog holds only the *schema* of what an activation must supply.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_catalog (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            name               TEXT    NOT NULL UNIQUE,
            scope              TEXT    NOT NULL,                    -- 'per_user' | 'global'
            source             TEXT    NOT NULL,                    -- 'remote' | 'local_script'
            transport          TEXT    NOT NULL DEFAULT 'stdio',
            command            TEXT,
            args_json          TEXT,
            env_json           TEXT,
            url                TEXT,
            script_path        TEXT,                               -- local_script: source under ./scripts
            config_schema_json TEXT,                               -- names of env/secret keys the UI must collect
            auth_kind          TEXT    NOT NULL DEFAULT 'none',    -- 'none'|'api_key'|'oauth'|'qr'|'ssh_key'
            role_filter        TEXT,                               -- JSON array of role ids; NULL = all
            friendly_name      TEXT,
            description        TEXT,
            created_at         TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    // Concrete globally-active connectors (shared, stateless — web-search etc.).
    // They run on the HOST. The global secret (admin's API key) is fine here:
    // `system.db` is admin-owned (§4/§15b). `catalog_name` is a registry→registry
    // FK (both in this file) — allowed.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_global_servers (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            name          TEXT    NOT NULL UNIQUE,
            catalog_name  TEXT    REFERENCES mcp_catalog(name),
            transport     TEXT    NOT NULL DEFAULT 'stdio',
            command       TEXT,
            args_json     TEXT,
            env_json      TEXT,
            url           TEXT,
            api_key       TEXT,
            friendly_name TEXT,
            description   TEXT,
            enabled       INTEGER NOT NULL DEFAULT 1,
            created_at    TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    // Which users may use each globally-active connector (§15 per-user access).
    // Mirrors `shared_folder_members`: both FKs are registry→registry, allowed.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_global_access (
            server_id INTEGER NOT NULL REFERENCES mcp_global_servers(id) ON DELETE CASCADE,
            user_id   TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            PRIMARY KEY (server_id, user_id)
        )",
    )
    .execute(pool)
    .await?;

    // Capability grants per role (blueprint §14). A single indexed lookup instead
    // of parsing `roles.attrs`. `admin` implicitly holds every capability (checked
    // in code), so only non-admin roles need rows here.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS role_capabilities (
            role_id    TEXT NOT NULL REFERENCES roles(id) ON DELETE CASCADE,
            capability TEXT NOT NULL,
            PRIMARY KEY (role_id, capability)
        )",
    )
    .execute(pool)
    .await?;

    Ok(())
}

// ── Owner tables ──────────────────────────────────────────────────────────────
//
// One owner's content. The schema is identical in every file that has it — only
// the file says who owns the rows — so this runs verbatim against `system.db`
// and against each `database/{userid}.db`.
//
// **No foreign key here may point at a registry table.** SQLite cannot enforce a
// key across files, not even through `ATTACH`, and sqlx turns on
// `PRAGMA foreign_keys`: the `CREATE TABLE` would succeed and every `INSERT`
// would fail. `create_owner_tables_stand_alone` in the tests guards this by
// running the schema against a file that has nothing else in it.

pub async fn create_owner_tables(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chat_sessions (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            title            TEXT,
            source           TEXT    NOT NULL DEFAULT 'web',
            agent_id         TEXT    NOT NULL DEFAULT 'main',
            is_interactive   INTEGER NOT NULL DEFAULT 1,
            is_ephemeral     INTEGER NOT NULL DEFAULT 0,
            run_context      TEXT,
            created_at       TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chat_sessions_stack (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id          INTEGER NOT NULL REFERENCES chat_sessions(id),
            agent_id            TEXT    NOT NULL DEFAULT 'main',
            agent_prompt        TEXT,
            depth               INTEGER NOT NULL DEFAULT 0,
            parent_tool_call_id INTEGER,
            terminated_at       TEXT,
            created_at          TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    // `model_db_id` used to point at `llm_models(id)`. It was write-only — never
    // selected, never joined — and it was the one key that crossed into the
    // registry. Which model answered is already recorded in
    // `llm_requests.model_name`.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chat_history (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            session_stack_id INTEGER NOT NULL REFERENCES chat_sessions_stack(id),
            role             TEXT    NOT NULL CHECK(role IN ('user', 'assistant', 'agent')),
            content          TEXT    NOT NULL DEFAULT '',
            status           TEXT    NOT NULL DEFAULT 'ok' CHECK(status IN ('ok', 'failed')),
            input_tokens     INTEGER,
            output_tokens    INTEGER,
            duration_ms      INTEGER,
            is_synthetic     INTEGER NOT NULL DEFAULT 0,
            reasoning_content TEXT,
            cost             REAL,
            metadata         TEXT,
            created_at       TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chat_llm_tools (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            message_id INTEGER NOT NULL REFERENCES chat_history(id),
            name       TEXT    NOT NULL,
            arguments  TEXT,
            result     TEXT,
            status     TEXT    NOT NULL DEFAULT 'running' CHECK(status IN ('running', 'pending', 'done', 'failed', 'cancelled', 'rejected')),
            result_type TEXT    NOT NULL DEFAULT 'string' CHECK(result_type IN ('string', 'json')),
            created_at TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_stack_session   ON chat_sessions_stack(session_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_history_stack   ON chat_history(session_stack_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_tools_message   ON chat_llm_tools(message_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS chat_summaries (
            id                      INTEGER PRIMARY KEY AUTOINCREMENT,
            stack_id                INTEGER NOT NULL REFERENCES chat_sessions_stack(id),
            content                 TEXT    NOT NULL,
            covers_up_to_message_id INTEGER NOT NULL,
            created_at              TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_chat_summaries_stack
         ON chat_summaries (stack_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS session_scratchpad (
            session_id INTEGER NOT NULL REFERENCES chat_sessions(id),
            key        TEXT    NOT NULL,
            value      TEXT    NOT NULL,
            PRIMARY KEY (session_id, key)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS session_mcp_grants (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id INTEGER NOT NULL,
            mcp_name   TEXT    NOT NULL,
            granted_at TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE(session_id, mcp_name)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS stack_mcp_grants (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            stack_id   INTEGER NOT NULL,
            mcp_name   TEXT    NOT NULL,
            granted_at TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE(stack_id, mcp_name)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS scheduled_jobs (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            title              TEXT    NOT NULL,
            description        TEXT    NOT NULL DEFAULT '',
            cron               TEXT    NOT NULL,
            prompt             TEXT    NOT NULL,
            agent_id           TEXT    NOT NULL DEFAULT 'main',
            session_id         INTEGER REFERENCES chat_sessions(id),
            enabled            INTEGER NOT NULL DEFAULT 1,
            last_run_at        TEXT,
            next_run_at        TEXT,
            single_run         INTEGER NOT NULL DEFAULT 0,
            running_session_id INTEGER,
            kind               TEXT    NOT NULL DEFAULT 'cron',
            parent_session_id  INTEGER REFERENCES chat_sessions(id),
            run_context        TEXT,
            running_since      TEXT,
            origin_ref         TEXT,
            created_at         TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS job_runs (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            job_id         INTEGER NOT NULL REFERENCES scheduled_jobs(id),
            session_id     INTEGER,
            started_at     TEXT    NOT NULL,
            completed_at   TEXT,
            duration_ms    INTEGER,
            status         TEXT    NOT NULL
                               CHECK(status IN ('completed', 'failed', 'cancelled')),
            final_response TEXT,
            error          TEXT,
            created_at     TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_job_runs_job_id
         ON job_runs (job_id, created_at DESC)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_events (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            source       TEXT    NOT NULL,
            method       TEXT    NOT NULL,
            payload      TEXT    NOT NULL,
            processed    INTEGER NOT NULL DEFAULT 0,
            processed_at TEXT,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_mcp_events_pending
         ON mcp_events (processed, created_at)
         WHERE processed = 0",
    )
    .execute(pool)
    .await?;

    // A user's activated per-user connectors (blueprint §7/§14). Owner table:
    // encrypted at rest in `{userid}.db`, so `api_key` (a personal secret / OAuth
    // refresh token) needs no column-level crypto. `catalog_name` is a BARE `TEXT`
    // snapshot of `mcp_catalog.name`, never a FK — an owner→registry key would pass
    // CREATE TABLE and fail every INSERT under `PRAGMA foreign_keys=ON` in an
    // isolated file (guarded by `owner_tables_stand_alone_with_foreign_keys_on`).
    // Local-script connectors run INSIDE the user's container against a script
    // copied into the bind-mounted home (`script_rel_path`).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_user_servers (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            name            TEXT    NOT NULL UNIQUE,
            catalog_name    TEXT,                              -- bare ref to mcp_catalog.name; NULL = self-registered remote
            source          TEXT    NOT NULL,                  -- 'remote' | 'local_script'
            transport       TEXT    NOT NULL DEFAULT 'stdio',
            command         TEXT,
            args_json       TEXT,
            env_json        TEXT,
            url             TEXT,
            api_key         TEXT,                              -- per-user secret / OAuth refresh token
            script_rel_path TEXT,                              -- container path for a local_script
            auth_state      TEXT    NOT NULL DEFAULT 'ready',  -- 'pending' | 'ready'
            enabled         INTEGER NOT NULL DEFAULT 1,
            created_at      TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sources (
            id                TEXT    PRIMARY KEY,
            active_session_id INTEGER REFERENCES chat_sessions(id),
            updated_at        TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS secrets (
            key        TEXT PRIMARY KEY,
            value      TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS projects (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            name        TEXT    NOT NULL,
            path        TEXT    NOT NULL,
            description TEXT    NOT NULL DEFAULT '',
            run_context TEXT,
            created_at  TEXT    NOT NULL DEFAULT (datetime('now')),
            updated_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS project_tickets (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            project_id   INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            title        TEXT    NOT NULL,
            description  TEXT    NOT NULL DEFAULT '',
            status       TEXT    NOT NULL DEFAULT 'todo'
                             CHECK(status IN ('todo','pending','in_progress','done','failed')),
            agent_id     TEXT    NOT NULL DEFAULT 'main',
            run_context  TEXT,
            job_id       INTEGER REFERENCES scheduled_jobs(id),
            result       TEXT,
            error        TEXT,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now')),
            started_at   TEXT,
            completed_at TEXT
        )",
    )
    .execute(pool)
    .await?;

    // Full request/response payloads for telemetry. Lives in the owner bucket
    // (per-user, encrypted) because it is conversation content. Correlated with
    // the metadata row in `system.db` via `request_id` (uuid).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS llm_request_payloads (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            request_id       TEXT    NOT NULL,
            request_json     TEXT    NOT NULL DEFAULT '',
            request_headers  TEXT,
            response_json    TEXT,
            response_headers TEXT,
            created_at       TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    // Backing store for the virtual `memory/` namespace (blueprint §5). MD-only
    // notes keyed by a path *relative to the namespace root* — the file is the
    // namespace, so no `memory/{userid}` / `memory/shared` prefix is stored:
    // routing picks the pool, the row keeps only the tail. Because this is an
    // owner table, the same schema backs private memory in each `{userid}.db`
    // (behind SQLCipher) and shared memory in `system.db` (cleartext, the
    // household owner) — §5.1. `path` is UNIQUE, so a write is an upsert.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS memory_docs (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            path       TEXT    NOT NULL UNIQUE,
            content    TEXT    NOT NULL DEFAULT '',
            created_at TEXT    NOT NULL DEFAULT (datetime('now')),
            updated_at TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    // Full-text index over memory notes: the payoff of a SQLite backing over
    // opaque file blobs (§5) — a decrypted session can search / RAG its own
    // memory. External-content FTS5 keeps no second copy of `content`; the
    // triggers below mirror every change from `memory_docs`. FTS5 is compiled
    // into the bundled SQLCipher build, so this works inside encrypted user
    // files too.
    sqlx::query(
        "CREATE VIRTUAL TABLE IF NOT EXISTS memory_docs_fts USING fts5(
            path, content,
            content='memory_docs',
            content_rowid='id'
        )",
    )
    .execute(pool)
    .await?;

    for trigger in [
        "CREATE TRIGGER IF NOT EXISTS memory_docs_ai AFTER INSERT ON memory_docs BEGIN
            INSERT INTO memory_docs_fts(rowid, path, content)
            VALUES (new.id, new.path, new.content);
         END",
        "CREATE TRIGGER IF NOT EXISTS memory_docs_ad AFTER DELETE ON memory_docs BEGIN
            INSERT INTO memory_docs_fts(memory_docs_fts, rowid, path, content)
            VALUES ('delete', old.id, old.path, old.content);
         END",
        "CREATE TRIGGER IF NOT EXISTS memory_docs_au AFTER UPDATE ON memory_docs BEGIN
            INSERT INTO memory_docs_fts(memory_docs_fts, rowid, path, content)
            VALUES ('delete', old.id, old.path, old.content);
            INSERT INTO memory_docs_fts(rowid, path, content)
            VALUES (new.id, new.path, new.content);
         END",
    ] {
        sqlx::query(trigger).execute(pool).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("skald-db-{tag}-{}-{nanos}", std::process::id()));
        p
    }

    /// The guardrail the file split would have given for free.
    ///
    /// `create_owner_tables` runs here against a database that holds *nothing
    /// else* — no `llm_models`, no `users`. Every table then takes a row. Any
    /// foreign key that reaches into a registry table passes `CREATE TABLE` and
    /// dies on the `INSERT`, right here, instead of lying dormant until a real
    /// user logs in and writes to their own file.
    #[tokio::test]
    async fn owner_tables_stand_alone_with_foreign_keys_on() {
        let dir = temp_dir("owner-standalone");
        let path = dir.join("owner.db");
        let pool = create_user_pool(&path, None).await.unwrap();

        let (fk,): (i64,) = sqlx::query_as("PRAGMA foreign_keys")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(fk, 1, "the guardrail is meaningless without FK enforcement");

        let one = |q: &'static str| sqlx::query(q).execute(&pool);

        one("INSERT INTO chat_sessions (id, title) VALUES (1, 't')").await.unwrap();
        one("INSERT INTO chat_sessions_stack (id, session_id) VALUES (1, 1)").await.unwrap();
        one("INSERT INTO chat_history (id, session_stack_id, role, content) VALUES (1, 1, 'user', 'hi')")
            .await.unwrap();
        one("INSERT INTO chat_llm_tools (message_id, name) VALUES (1, 'exec')").await.unwrap();
        one("INSERT INTO chat_summaries (stack_id, content, covers_up_to_message_id) VALUES (1, 's', 1)")
            .await.unwrap();
        one("INSERT INTO session_scratchpad (session_id, key, value) VALUES (1, 'k', 'v')").await.unwrap();
        one("INSERT INTO session_mcp_grants (session_id, mcp_name) VALUES (1, 'm')").await.unwrap();
        one("INSERT INTO stack_mcp_grants (stack_id, mcp_name) VALUES (1, 'm')").await.unwrap();
        one("INSERT INTO scheduled_jobs (id, title, cron, prompt, session_id) VALUES (1, 't', '* * * * *', 'p', 1)")
            .await.unwrap();
        one("INSERT INTO job_runs (job_id, started_at, status) VALUES (1, 'now', 'completed')").await.unwrap();
        // Owner table with a BARE `catalog_name` ref — proves it stands alone with
        // FKs on (an owner→registry FK here would die on this INSERT).
        one("INSERT INTO mcp_user_servers (name, catalog_name, source) VALUES ('u', 'whatsapp', 'local_script')").await.unwrap();
        one("INSERT INTO mcp_events (source, method, payload) VALUES ('s', 'm', '{}')").await.unwrap();
        one("INSERT INTO sources (id, active_session_id) VALUES ('web', 1)").await.unwrap();
        one("INSERT INTO secrets (key, value) VALUES ('k', 'v')").await.unwrap();
        one("INSERT INTO projects (id, name, path) VALUES (1, 'p', '/tmp')").await.unwrap();
        one("INSERT INTO project_tickets (project_id, title, job_id) VALUES (1, 't', 1)").await.unwrap();
        one("INSERT INTO llm_request_payloads (request_id, request_json) VALUES ('r1', '{}')").await.unwrap();
        // Fires the AFTER INSERT trigger into the external-content FTS5 table.
        one("INSERT INTO memory_docs (path, content) VALUES ('notes/x.md', 'hello world')").await.unwrap();

        // ...and the FTS index actually answers a MATCH.
        let (hits,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM memory_docs_fts WHERE memory_docs_fts MATCH 'world'",
        )
        .fetch_one(&pool).await.unwrap();
        assert_eq!(hits, 1, "memory_docs_fts must index inserted notes");

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cleartext user's database is an ordinary SQLite file; an encrypted one
    /// is unreadable without the key, and does not even carry SQLite's header.
    #[tokio::test]
    async fn an_encrypted_database_is_opaque_without_its_key() {
        let dir = temp_dir("opaque");
        let clear_path = dir.join("clear.db");
        let enc_path = dir.join("enc.db");

        let clear = create_user_pool(&clear_path, None).await.unwrap();
        clear.close().await;

        let dek = Dek::random();
        let enc = create_user_pool(&enc_path, Some(&dek)).await.unwrap();
        sqlx::query("INSERT INTO chat_sessions (title) VALUES ('secret')")
            .execute(&enc).await.unwrap();
        enc.close().await;

        let magic = b"SQLite format 3\0";
        assert_eq!(&std::fs::read(&clear_path).unwrap()[..16], magic);
        assert_ne!(&std::fs::read(&enc_path).unwrap()[..16], magic,
                   "an encrypted file must not advertise itself as SQLite");

        assert!(open_user_pool(&enc_path, None).await.is_err(), "no key must not open it");
        assert!(open_user_pool(&enc_path, Some(&Dek::random())).await.is_err(), "wrong key must not open it");

        // ...and the right key still does, with the row intact.
        let reopened = open_user_pool(&enc_path, Some(&dek)).await.unwrap();
        let (title,): (String,) = sqlx::query_as("SELECT title FROM chat_sessions")
            .fetch_one(&reopened).await.unwrap();
        assert_eq!(title, "secret");
        reopened.close().await;

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Login must never conjure a database. A missing file is data loss and has
    /// to be reported, not papered over with a fresh empty one.
    #[tokio::test]
    async fn open_never_creates_a_database() {
        let dir = temp_dir("no-create");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ghost.db");

        assert!(open_user_pool(&path, None).await.is_err());
        assert!(open_user_pool(&path, Some(&Dek::random())).await.is_err());
        assert!(!path.exists(), "open_user_pool must not have created the file");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
