pub mod access_defaults;
pub mod activated_tools;
pub mod approval_rules;
pub mod project_members;
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
pub mod mcp_catalog_access;
pub mod mcp_events;
pub mod mcp_global_access;
pub mod mcp_global_servers;
pub mod mcp_user_servers;
pub mod memory_docs;
pub mod oauth_providers;
pub mod plugins;
pub mod plugin_access;
pub mod plugin_user_configs;
pub mod reports;
pub mod role_capabilities;
pub mod roles;
pub mod scheduled_jobs;
pub mod scratchpad;
pub mod shared_folders;
pub mod sources;
pub mod supervision;
pub mod system_agent_coverage;
pub mod system_agent_runs;
pub mod system_agent_state;
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

/// Adds a nullable column if it is not already present, so a purely **additive**
/// schema change lands on an existing database without a wipe. Greenfield still
/// permits a clean recreate (§0); this only spares an existing box's data when the
/// change is additive, and is a no-op on a fresh DB where the column already exists
/// in the `CREATE TABLE`. The "duplicate column name" error means it's already there.
async fn ensure_column(pool: &SqlitePool, table: &str, column: &str, decl: &str) -> Result<()> {
    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {decl}");
    if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(sql)).execute(pool).await {
        if !e.to_string().contains("duplicate column name") {
            return Err(e.into());
        }
    }
    Ok(())
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
            id                TEXT    PRIMARY KEY,
            enabled           INTEGER NOT NULL DEFAULT 0,
            config            TEXT    NOT NULL DEFAULT '{}',
            grant_by_default  INTEGER NOT NULL DEFAULT 1,
            created_at        TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;
    // See `db::access_defaults`: the default audience of a newly-created object.
    // Additive so an existing box keeps its rows — and inherits the open default,
    // which only matters for *future* users (existing ones keep their grants).
    ensure_column(pool, "plugins", "grant_by_default", "INTEGER NOT NULL DEFAULT 1").await?;

    // Which users may see/configure each plugin. `plugin_id` is deliberately
    // NOT a foreign key to plugins.id: plugin identity comes from compiled
    // registration, and a `plugins` row is only created lazily on first
    // toggle — a plugin never configured must still be grantable.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS plugin_access (
            plugin_id  TEXT NOT NULL,
            user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            created_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (plugin_id, user_id)
        )",
    )
    .execute(pool)
    .await?;

    // Per-user plugin settings (e.g. Telegram's pairing status). Lives in
    // `system.db` — admin-readable, never secrets. `plugin_id` not a FK for
    // the same reason as plugin_access.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS plugin_user_configs (
            plugin_id  TEXT NOT NULL,
            user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            config     TEXT NOT NULL DEFAULT '{}',
            updated_at TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (plugin_id, user_id)
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
            locale            TEXT,
            birthdate         TEXT,
            sex               TEXT,
            notes             TEXT,
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
    // Per-user UI locale override is additive — reaches an existing DB in place.
    ensure_column(pool, "users", "locale", "TEXT").await?;
    // Admin-managed directory profile fields (§0.1-neutral): `birthdate` is an
    // ISO YYYY-MM-DD date, `sex` free text, `notes` admin-authored. Rendered
    // into agent prompts by the `__USER_PROFILE__` substitution. Additive.
    ensure_column(pool, "users", "birthdate", "TEXT").await?;
    ensure_column(pool, "users", "sex", "TEXT").await?;
    ensure_column(pool, "users", "notes", "TEXT").await?;

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
            description TEXT    NOT NULL DEFAULT '',
            created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;
    // The folder's description is injected into the agent's system context so it
    // knows what each shared folder holds and when to read/write it. Additive —
    // reaches an existing DB in place (a no-op on the fresh CREATE above).
    ensure_column(pool, "shared_folders", "description", "TEXT NOT NULL DEFAULT ''").await?;

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

    // ── Projects: shareable endeavours over an on-disk folder (blueprint §5 memory
    // note / §6). A project is an *endeavour* with an owner + membership that HAS a
    // *place*: a folder `{WD}/projects/{owner_userid}/{slug}` bind-mounted into each
    // member's container (like a shared folder, but two path segments — owner + slug).
    // Registry tables — metadata is NOT encrypted (only user↔agent conversations are);
    // this lets a project be shared across members without the cross-DB-FK problem that
    // an owner-bucket table would hit. `owner_user_id → users(id)` is registry→registry.
    // The owner is also inserted as a `project_members` row (can_write=1) so mounts are
    // uniform (a private project = a project with one member).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS projects (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            owner_user_id TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            name          TEXT    NOT NULL,
            slug          TEXT    NOT NULL,
            description   TEXT    NOT NULL DEFAULT '',
            run_context   TEXT,
            created_at    TEXT    NOT NULL DEFAULT (datetime('now')),
            updated_at    TEXT    NOT NULL DEFAULT (datetime('now')),
            UNIQUE (owner_user_id, slug)
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS project_members (
            project_id INTEGER NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
            user_id    TEXT    NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            can_write  INTEGER NOT NULL DEFAULT 1,
            PRIMARY KEY (project_id, user_id)
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
            script_path        TEXT,                               -- local_script: entry file, as <connector>/<file> under ./connectors
            config_schema_json TEXT,                               -- env[] entries (objects) the UI must collect
            auth_kind          TEXT    NOT NULL DEFAULT 'none',    -- 'none'|'api_key'|'oauth'|'qr'|'ssh_key'
            oauth_provider     TEXT,                               -- oauth: slug into oauth_providers.name
            oauth_scopes_json  TEXT,                               -- oauth: JSON array of scopes requested at consent
            deliver_json       TEXT,                               -- oauth: {as,format,env,path} credential delivery spec
            role_filter        TEXT,                               -- JSON array of role ids; NULL = all
            verify_command     TEXT,                               -- shell command run before persisting an activation
            verify_script_path TEXT,                               -- script file the verify command references, if any
            icon_small_path    TEXT,                               -- icon file inside ./connectors/<name>/, if the feed shipped one
            icon_large_path    TEXT,
            friendly_name      TEXT,
            description        TEXT,
            tool_meta_json     TEXT,                               -- [{name,display_name}] friendly tool names from the manifest
            version            INTEGER,                            -- marketplace build number: the update-comparison key
            version_string     TEXT,                               -- semver, display only
            version_release_date TEXT,                             -- ISO date, display only
            grant_by_default   INTEGER NOT NULL DEFAULT 1,        -- auto-grant to auto-grant roles (db::access_defaults)
            created_at         TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;
    ensure_column(pool, "mcp_catalog", "grant_by_default", "INTEGER NOT NULL DEFAULT 1").await?;
    // OAuth columns are additive (§15) — reach an already-created catalog in place.
    ensure_column(pool, "mcp_catalog", "oauth_provider",    "TEXT").await?;
    ensure_column(pool, "mcp_catalog", "oauth_scopes_json", "TEXT").await?;
    ensure_column(pool, "mcp_catalog", "deliver_json",      "TEXT").await?;
    // Manifest-declared friendly tool names (UI card titles) — additive.
    ensure_column(pool, "mcp_catalog", "tool_meta_json",    "TEXT").await?;
    // Versioning columns are additive: the installed `version` integer is compared
    // against the feed's to surface "update available" in the marketplace UI.
    ensure_column(pool, "mcp_catalog", "version",              "INTEGER").await?;
    ensure_column(pool, "mcp_catalog", "version_string",       "TEXT").await?;
    ensure_column(pool, "mcp_catalog", "version_release_date", "TEXT").await?;

    // Concrete globally-active connectors (shared, stateless — web-search etc.).
    // They run on the HOST. The global secret (admin's API key) is fine here:
    // `system.db` is admin-owned (§4/§15b). `catalog_name` is a registry→registry
    // FK (both in this file) — allowed.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_global_servers (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            name               TEXT    NOT NULL UNIQUE,
            catalog_name       TEXT    REFERENCES mcp_catalog(name),
            transport          TEXT    NOT NULL DEFAULT 'stdio',
            command            TEXT,
            args_json          TEXT,
            env_json           TEXT,
            url                TEXT,
            api_key            TEXT,
            verify_command     TEXT,                               -- snapshot of mcp_catalog.verify_command
            verify_script_path TEXT,                               -- absolute host path of the verify script, if any
            friendly_name      TEXT,
            description        TEXT,
            enabled            INTEGER NOT NULL DEFAULT 1,
            grant_by_default   INTEGER NOT NULL DEFAULT 1,        -- auto-grant to auto-grant roles (db::access_defaults)
            created_at         TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;
    ensure_column(pool, "mcp_global_servers", "grant_by_default", "INTEGER NOT NULL DEFAULT 1").await?;

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

    // Which users the admin has authorized to activate each per-user catalog
    // connector (the catalog twin of `mcp_global_access`; deny-by-default — no row
    // = no access). `catalog_name` FK is registry→registry (both in this file),
    // allowed. Supersedes `mcp_catalog.role_filter` as the access gate.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS mcp_catalog_access (
            catalog_name TEXT NOT NULL REFERENCES mcp_catalog(name) ON DELETE CASCADE,
            user_id      TEXT NOT NULL REFERENCES users(id)         ON DELETE CASCADE,
            PRIMARY KEY (catalog_name, user_id)
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

    // OAuth providers for per-user connectors (blueprint §15). One row per identity
    // provider (Google, …), referenced by name from `mcp_catalog.oauth_provider`. A
    // single app covers every service of that provider (Gmail, Calendar, Drive) —
    // client credentials are keyed on the provider, scopes on the connector.
    //
    // Registry table (`system.db`): `client_secret` is a household/global secret the
    // admin owns (§4/§15b), not a per-user one, so it belongs here in the admin-
    // readable file. The per-user refresh tokens each activation obtains never land
    // here — they go, encrypted, into the user's `mcp_user_servers.api_key`.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS oauth_providers (
            name          TEXT PRIMARY KEY,                        -- slug referenced by mcp_catalog.oauth_provider
            display_name  TEXT NOT NULL,
            auth_url      TEXT NOT NULL,                           -- authorization endpoint
            token_url     TEXT NOT NULL,                           -- token endpoint
            client_id     TEXT NOT NULL,
            client_secret TEXT NOT NULL,
            redirect_uri  TEXT NOT NULL,                           -- copy-paste landing page (oauth/show.html)
            extra_params  TEXT,                                    -- JSON of extra auth params (access_type, prompt, …)
            created_at    TEXT NOT NULL DEFAULT (datetime('now')),
            updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    // The supervision edge (§0.1): one person's activity may be read on another's
    // behalf. **A generic edge between two users, and nothing more** — the domain
    // reading of it ("a parent watches a child") lives in the seed data and the UI
    // copy, never here, so a pivot to a mentor watching a trainee, or a care worker
    // watching a resident, renames nothing.
    //
    // It answers two questions with one table, which is why it is an edge and not a
    // per-agent list of subjects: *whom does a background agent look at* (the
    // distinct subjects) and *who may read what it produced* (the supervisors of a
    // given subject). The second is what the reports' `audience = 'supervisors'`
    // resolves against.
    //
    // Both FKs are registry→registry (same file), so they are allowed and the
    // cascade is real: deleting a user takes their edges with them, in both
    // directions.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS supervision (
            subject_user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            supervisor_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            created_at         TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (subject_user_id, supervisor_user_id)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_supervision_supervisor
         ON supervision(supervisor_user_id)",
    )
    .execute(pool)
    .await?;

    // How far a background agent has *processed* a subject — the watermark that
    // makes "everything since last time" a well-defined window.
    //
    // **Not `system_agent_runs`, and not `system_agent_state`**, though it sits
    // between them and the difference is the whole reason it exists:
    //
    //   `system_agent_state`  when an agent last *attempted* a pass. Advances on
    //                         every tick, including idle ones, and is marked
    //                         *before* the work — so it can never delimit the
    //                         window the work is about.
    //   this table            how far the work actually got. Advances **only on a
    //                         completed pass**, so a crash mid-pass re-covers the
    //                         same stretch next time. For a review, a duplicate
    //                         report is a nuisance and a skipped window is a blind
    //                         spot: at-least-once is the only acceptable direction.
    //
    // The obvious alternative — deriving the watermark from the last report's
    // `period_end` — fails on a single ordinary action: a supervisor deleting an
    // old report would move the scheduler's window back and regenerate the very
    // report they discarded. A document is the user's to delete; scheduler state is
    // not, so they cannot be the same row.
    //
    // Registry, not owner, for a reason specific to how these passes run: the pass
    // executes inside *some* supervisor's runtime, and which one depends on who is
    // logged in tonight. A watermark in the acting user's file would give one
    // subject two unsynchronised clocks.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS system_agent_coverage (
            agent_id        TEXT NOT NULL,
            subject_user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            covered_through TEXT NOT NULL,                          -- UTC 'YYYY-MM-DD HH:MM:SS'
            updated_at      TEXT NOT NULL DEFAULT (datetime('now')),
            PRIMARY KEY (agent_id, subject_user_id)
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
            preview_old TEXT,                                -- file-write diff: content before the write
            preview_new TEXT,                                -- file-write diff: content after the write
            media       TEXT,                                -- JSON [{host_path,mime}]: media the tool produced, inlined to the model out of band
            created_at TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;
    // Diff-preview + tool-media columns are additive — reach an already-created table in place.
    ensure_column(pool, "chat_llm_tools", "preview_old", "TEXT").await?;
    ensure_column(pool, "chat_llm_tools", "preview_new", "TEXT").await?;
    ensure_column(pool, "chat_llm_tools", "media", "TEXT").await?;

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

    // Tool-group activations — the durable **effect** of `activate_tools`. One
    // row per activated group, anchored at the assistant `message_id` that
    // triggered it. `stack_id IS NULL` = session-scoped (root agent); non-NULL =
    // sub-agent frame (removed on frame exit). `kind`/`ref`: ('builtin','config')
    // or ('mcp', <server>). All FKs are owner→owner.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS activated_tools (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id INTEGER NOT NULL REFERENCES chat_sessions(id),
            stack_id   INTEGER          REFERENCES chat_sessions_stack(id),
            message_id INTEGER NOT NULL REFERENCES chat_history(id),
            kind       TEXT    NOT NULL CHECK(kind IN ('builtin', 'mcp')),
            ref        TEXT    NOT NULL,
            created_at TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;
    // Dedup per scope. A plain UNIQUE(session_id, stack_id, kind, ref) would NOT
    // dedup session-scoped rows: SQLite treats two NULL `stack_id`s as distinct,
    // so INSERT OR IGNORE would pile up duplicates. COALESCE folds NULL to -1.
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS ux_activated_tools
         ON activated_tools(session_id, COALESCE(stack_id, -1), kind, ref)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_activated_tools_stack ON activated_tools(stack_id)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_activated_tools_msg ON activated_tools(message_id)",
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

    // Execution log of the **system agents** — the background agents the instance
    // runs on a user's behalf without being asked (event triage is the first and,
    // today, the only one). The sibling of `job_runs`: same shape, but keyed on the
    // agent instead of a scheduled job, because a system agent has no user-authored
    // row to point at.
    //
    // Owner table, and that is the whole privacy story: an event-triage run
    // summarises what landed in this user's inbox, so it belongs in *their*
    // encrypted file and nowhere else. There is deliberately no `user_id` column — the file is
    // the owner (§5.1). An admin reading `system.db` learns nothing about it.
    //
    // A user whose database is still locked is skipped by the scheduler and
    // produces no row at all: the only file that could hold it is the one we
    // cannot open. Hence no 'skipped' status — the skip is a log line (§9).
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS system_agent_runs (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id     TEXT    NOT NULL,
            session_id   INTEGER,
            started_at   TEXT    NOT NULL,
            completed_at TEXT,
            duration_ms  INTEGER,
            status       TEXT    NOT NULL
                             CHECK(status IN ('running', 'completed', 'failed', 'cancelled')),
            stats        TEXT,
            error        TEXT,
            created_at   TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_system_agent_runs_agent
         ON system_agent_runs (agent_id, created_at DESC)",
    )
    .execute(pool)
    .await?;

    // When each system agent last *attempted* a pass for this user — the
    // scheduler's state, deliberately kept apart from `system_agent_runs`.
    //
    // The two answer different questions and conflating them breaks both. The run
    // log is a history for the human: an idle tick writes nothing there, or it
    // degenerates into a heartbeat. Scheduling needs the opposite — every attempt,
    // productive or not — because "is this agent due?" is `now - last_attempt >=
    // interval`. Reading due-ness off the run log would re-run an idle agent on
    // every pass, and a weekly agent would never come due at all once its last
    // productive run aged out.
    //
    // Persisting it is what makes a long interval survive a restart. An in-memory
    // deadline is fine at event triage's scale — a few minutes, re-armed on boot —
    // but a weekly agent on a machine rebooted every few days would have its deadline
    // reset before it ever fired, and would simply never run.
    //
    // Owner table for the same reason as the run log: when an agent last ran for
    // someone is that person's activity, not the registry's.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS system_agent_state (
            agent_id        TEXT PRIMARY KEY,
            last_attempt_at TEXT NOT NULL
        )",
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
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            name                TEXT    NOT NULL UNIQUE,
            catalog_name        TEXT,                              -- bare ref to mcp_catalog.name; NULL = self-registered remote
            source              TEXT    NOT NULL,                  -- 'remote' | 'local_script'
            transport           TEXT    NOT NULL DEFAULT 'stdio',
            command             TEXT,
            args_json           TEXT,
            env_json            TEXT,
            url                 TEXT,
            api_key             TEXT,                              -- per-user secret / OAuth refresh token
            oauth_provider      TEXT,                              -- oauth: snapshot of catalog oauth_provider
            deliver_json        TEXT,                              -- oauth: snapshot of catalog credential delivery spec
            script_rel_path     TEXT,                              -- container path for a local_script
            verify_command      TEXT,                              -- snapshot of mcp_catalog.verify_command
            verify_script_rel_path TEXT,                          -- container path of the verify script, if any
            auth_state          TEXT    NOT NULL DEFAULT 'ready',  -- 'pending' | 'ready'
            enabled             INTEGER NOT NULL DEFAULT 1,
            created_at          TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;
    // OAuth columns are additive (§15) — reach an already-created table in place.
    ensure_column(pool, "mcp_user_servers", "oauth_provider", "TEXT").await?;
    ensure_column(pool, "mcp_user_servers", "deliver_json",   "TEXT").await?;

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

    // NOTE: `projects` + `project_members` are **registry** tables (see
    // `create_registry_tables`) — shareable, not encrypted. The old owner-bucket
    // `projects`/`project_tickets` tables (single-user Skald leftover) were removed
    // when projects became a shareable, container-mounted endeavour.

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

    // Reports — the documents system agents write about a stretch of time
    // (blueprint §13). Like `memory_docs` above, one owner schema backs **two
    // homes**, and which file a row lands in *is* its audience:
    //
    //   `{userid}.db`  a report that belongs to that user, about that user —
    //                  their weekly "what you struggled to get done" digest.
    //                  Behind SQLCipher: nobody else can read it, admin included.
    //   `system.db`    an instance report, written about someone *for* the
    //                  people who supervise them. Cleartext to whoever owns the
    //                  box, deliberately — they are the intended reader (§2).
    //
    // That split is why nothing here filters by reader: a report's subject can
    // never see an instance report about them, because their tools only ever
    // touch their own pool. The invisibility is structural, not a rule someone
    // has to remember in each query.
    //
    // The producer's scope decides the file with no extra concept:
    // `AgentScope::PerUser` writes into `ctx.pool`, `AgentScope::Instance` into
    // the registry pool the agent already holds.
    //
    // `subject_user_id` / `producer_user_id` / `run_id` are **bare** columns, not
    // foreign keys: `users` lives in the registry (an owner→registry FK would
    // fail every INSERT), and for an instance row the `system_agent_runs` trace
    // sits in the *acting* user's file. They are snapshots, and a deleted user
    // leaves them dangling on purpose — the report outlives the account.
    //
    // `kind` is free-form producer-declared text, never an enum (§0.1). Rows are
    // immutable once written: the only UPDATE is the read acknowledgement.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS reports (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            kind             TEXT    NOT NULL,                  -- producer-declared type, not an enum
            title            TEXT    NOT NULL,
            summary          TEXT,                              -- one line: lists + notification text
            body             TEXT    NOT NULL DEFAULT '',       -- markdown
            severity         TEXT    NOT NULL DEFAULT 'info',   -- 'info' | 'notice' | 'alert'
            subject_user_id  TEXT,                              -- who it is about (bare snapshot)
            audience         TEXT    NOT NULL DEFAULT 'owner',  -- 'owner' | 'admins' | 'supervisors'
            period_start     TEXT,                              -- the window it covers
            period_end       TEXT,
            produced_by      TEXT    NOT NULL,                  -- system agent id
            producer_user_id TEXT,                              -- whose runtime ran the pass
            run_id           INTEGER,                           -- system_agent_runs.id (bare snapshot)
            metadata         TEXT,                              -- JSON counters; never contents
            read_at          TEXT,                              -- shared acknowledgement: first reader wins
            read_by          TEXT,
            created_at       TEXT    NOT NULL DEFAULT (datetime('now'))
        )",
    )
    .execute(pool)
    .await?;

    // Listing is always newest-first, optionally narrowed to one subject. `kind`
    // is deliberately unindexed: a handful of rows a week means a scan is
    // cheaper than the index it would need.
    for index in [
        "CREATE INDEX IF NOT EXISTS idx_reports_created ON reports(created_at DESC, id DESC)",
        "CREATE INDEX IF NOT EXISTS idx_reports_subject ON reports(subject_user_id, created_at DESC)",
    ] {
        sqlx::query(index).execute(pool).await?;
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
        // Both activation scopes: session-scoped (stack_id NULL) + stack-scoped.
        one("INSERT INTO activated_tools (session_id, stack_id, message_id, kind, ref) VALUES (1, NULL, 1, 'mcp', 'm')").await.unwrap();
        one("INSERT INTO activated_tools (session_id, stack_id, message_id, kind, ref) VALUES (1, 1, 1, 'mcp', 'm')").await.unwrap();
        one("INSERT INTO scheduled_jobs (id, title, cron, prompt, session_id) VALUES (1, 't', '* * * * *', 'p', 1)")
            .await.unwrap();
        one("INSERT INTO job_runs (job_id, started_at, status) VALUES (1, 'now', 'completed')").await.unwrap();
        one("INSERT INTO system_agent_runs (agent_id, started_at, status) VALUES ('event-triage', 'now', 'running')").await.unwrap();
        // Owner table with a BARE `catalog_name` ref — proves it stands alone with
        // FKs on (an owner→registry FK here would die on this INSERT).
        one("INSERT INTO mcp_user_servers (name, catalog_name, source) VALUES ('u', 'whatsapp', 'local_script')").await.unwrap();
        one("INSERT INTO mcp_events (source, method, payload) VALUES ('s', 'm', '{}')").await.unwrap();
        one("INSERT INTO sources (id, active_session_id) VALUES ('web', 1)").await.unwrap();
        one("INSERT INTO secrets (key, value) VALUES ('k', 'v')").await.unwrap();
        one("INSERT INTO llm_request_payloads (request_id, request_json) VALUES ('r1', '{}')").await.unwrap();
        // Fires the AFTER INSERT trigger into the external-content FTS5 table.
        one("INSERT INTO memory_docs (path, content) VALUES ('notes/x.md', 'hello world')").await.unwrap();
        // Bare `subject_user_id` / `producer_user_id` (registry `users`) and a
        // bare `run_id` that points at no row in this file — an FK on any of the
        // three would die right here.
        one("INSERT INTO reports (kind, title, body, produced_by, subject_user_id, producer_user_id, run_id)
             VALUES ('conversation-review', 't', 'b', 'agent', 'u-absent', 'u-also-absent', 4242)").await.unwrap();

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

    /// `plugin_access` / `plugin_user_configs`: grant/revoke round-trip, JSON
    /// blob round-trip, and the `users(id)` cascade. `plugin_id` deliberately
    /// accepts ids with no `plugins` row (identity = compiled registration).
    #[tokio::test]
    async fn plugin_access_and_user_configs_round_trip() {
        let dir = temp_dir("plugin-tables");
        let path = dir.join("system.db");
        let pool = init_system_pool(path.to_str().unwrap()).await.unwrap();

        let mk_user = |id: &str| {
            sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES (?, ?, 'admin', 0)")
                .bind(id.to_string()).bind(id.to_string())
                .execute(&pool)
        };
        mk_user("u1").await.unwrap();
        mk_user("u2").await.unwrap();

        // No `plugins` row for "telegram" — grants must still work.
        plugin_access::grant(&pool, "telegram", "u1").await.unwrap();
        plugin_access::grant(&pool, "telegram", "u1").await.unwrap(); // idempotent
        plugin_access::grant(&pool, "telegram", "u2").await.unwrap();
        assert!(plugin_access::has_access(&pool, "telegram", "u1").await.unwrap());
        assert!(!plugin_access::has_access(&pool, "comfyui", "u1").await.unwrap());
        assert_eq!(plugin_access::plugin_ids_for_user(&pool, "u1").await.unwrap(), vec!["telegram"]);
        assert_eq!(plugin_access::users_for_plugin(&pool, "telegram").await.unwrap(), vec!["u1", "u2"]);

        plugin_access::set_access(&pool, "telegram", &["u2".to_string()]).await.unwrap();
        assert!(!plugin_access::has_access(&pool, "telegram", "u1").await.unwrap());
        assert_eq!(plugin_access::users_for_plugin(&pool, "telegram").await.unwrap(), vec!["u2"]);

        // The Users-page write path: one user's grants across every plugin. A
        // blanket replace, and scoped to that user — u2's telegram grant stands.
        plugin_access::set_for_user(&pool, "u1", &["comfyui".to_string(), "honcho".to_string()])
            .await.unwrap();
        assert_eq!(
            plugin_access::plugin_ids_for_user(&pool, "u1").await.unwrap(),
            vec!["comfyui", "honcho"],
        );
        assert!(plugin_access::has_access(&pool, "telegram", "u2").await.unwrap());
        plugin_access::set_for_user(&pool, "u1", &["honcho".to_string()]).await.unwrap();
        assert_eq!(plugin_access::plugin_ids_for_user(&pool, "u1").await.unwrap(), vec!["honcho"]);
        plugin_access::set_for_user(&pool, "u1", &[]).await.unwrap();
        assert!(plugin_access::plugin_ids_for_user(&pool, "u1").await.unwrap().is_empty());
        assert!(plugin_access::has_access(&pool, "telegram", "u2").await.unwrap());

        plugin_user_configs::set(&pool, "telegram", "u2", &serde_json::json!({"linked": true})).await.unwrap();
        assert_eq!(
            plugin_user_configs::get(&pool, "telegram", "u2").await.unwrap(),
            Some(serde_json::json!({"linked": true})),
        );
        plugin_user_configs::set(&pool, "telegram", "u2", &serde_json::json!({"linked": false})).await.unwrap();
        assert_eq!(
            plugin_user_configs::get(&pool, "telegram", "u2").await.unwrap(),
            Some(serde_json::json!({"linked": false})),
        );
        assert_eq!(plugin_user_configs::get(&pool, "telegram", "u1").await.unwrap(), None);

        // Deleting the user cascades both tables.
        sqlx::query("DELETE FROM users WHERE id = 'u2'").execute(&pool).await.unwrap();
        assert_eq!(plugin_access::users_for_plugin(&pool, "telegram").await.unwrap(), Vec::<String>::new());
        assert_eq!(plugin_user_configs::get(&pool, "telegram", "u2").await.unwrap(), None);

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `effective_access`: the runtime gate channels enforce. Admin holds every
    /// plugin implicitly (even one they were never granted); a member needs an
    /// explicit grant; an unknown user fails closed.
    #[tokio::test]
    async fn plugin_effective_access_admin_short_circuit_and_grants() {
        let dir = temp_dir("plugin-effective-access");
        let path = dir.join("system.db");
        let pool = init_system_pool(path.to_str().unwrap()).await.unwrap();

        // A non-admin role, plus one admin and one member user.
        sqlx::query("INSERT INTO roles (id, label, permission_group) VALUES ('member', 'Member', 'default')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES ('adm', 'adm', 'admin', 0)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO users (id, username, role_id, encrypted) VALUES ('mem', 'mem', 'member', 0)")
            .execute(&pool).await.unwrap();

        plugin_access::grant(&pool, "telegram", "mem").await.unwrap();

        // Admin: every plugin, granted or not.
        assert!(plugin_access::effective_access(&pool, "telegram", "adm").await.unwrap());
        assert!(plugin_access::effective_access(&pool, "comfyui",  "adm").await.unwrap());
        // Member: only what they were granted.
        assert!(plugin_access::effective_access(&pool, "telegram", "mem").await.unwrap());
        assert!(!plugin_access::effective_access(&pool, "comfyui", "mem").await.unwrap());
        // Unknown user → fail closed.
        assert!(!plugin_access::effective_access(&pool, "telegram", "ghost").await.unwrap());

        pool.close().await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
