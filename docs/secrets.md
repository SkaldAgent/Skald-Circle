# Secrets Store

Centralised key-value store for sensitive tokens and credentials (API keys,
HuggingFace tokens, etc.) that need to be shared across plugins and tools
without appearing in `config.yml` or plugin configs.

---

## Architecture

```text
crates/core-api/src/secrets.rs
  — SecretsApi trait  (full CRUD: get, set, delete, list_keys)
  — require()         (helper: get or bail with helpful error message)

src/secrets.rs
  — SecretsStore      (implements SecretsApi over SQLite)
```

`SecretsStore` holds an `Arc<SqlitePool>` and issues direct SQL queries — no
in-memory cache, no state. It is cheap to clone (just clones the pool Arc).

---

## Trait API (crates/core-api)

```rust
// core_api::secrets
#[async_trait]
pub trait SecretsApi: Send + Sync {
    async fn get(&self, key: &str) -> Option<String>;
    async fn set(&self, key: &str, value: &str) -> Result<()>;
    async fn delete(&self, key: &str) -> Result<()>;
    async fn list_keys(&self) -> Vec<String>;   // never returns values
}

// Convenience: returns the value or an anyhow error with instructions.
pub async fn require(secrets: &Arc<dyn SecretsApi>, key: &str) -> Result<String>;
```

---

## Access points

| Location | Field | Use |
|---|---|---|
| `Skald` | `secrets: Arc<SecretsStore>` | Agent tools, REST API handlers |
| `PluginContext` | `secrets: Arc<dyn SecretsApi>` | Plugin start/reload (read or write) |

Plugins read secrets at startup (e.g. to pass a token to a subprocess). The
agent writes secrets via its tools. Neither needs to depend on the main crate.

---

## Usage from a plugin

```rust
use core_api::secrets;

// require() fails with a helpful message if the secret is absent.
let token = secrets::require(&ctx.secrets, "HUGGINGFACE_TOKEN").await?;

// Or a soft check:
if let Some(token) = ctx.secrets.get("MY_API_KEY").await {
    // use token
}
```

---

## Agent tools

Two built-in tools let the agent manage secrets without exposing values:

| Tool | Parameters | Behaviour |
|---|---|---|
| `set_secret` | `key: string`, `value: string\|null` | Sets the secret. Empty string or null **deletes** the key. |
| `list_secrets` | `pattern?: string` | Returns keys that exist. Optional glob filter (e.g. `GOOGLE_*`). Never returns values. |

The agent can check whether a key is set by calling `list_secrets("KEY_NAME")` — if the key is absent from the result it has not been configured yet.

## Usage from Rust code

Agent tools receive `Arc<dyn SecretsApi>` from `Skald`:

```rust
skald.secrets.set("HUGGINGFACE_TOKEN", &value).await?;
skald.secrets.delete("OLD_KEY").await?;
let keys = skald.secrets.list_keys().await;  // safe to log
```

---

## Well-known keys

| Key | Used by |
|-----|---------|
| `HUGGINGFACE_TOKEN` | `plugin-tts-orpheus-3b` — passed as `HF_TOKEN` env var to the Python subprocess |

Add new rows here when a plugin or tool introduces a new well-known secret key.

---

## DB: secrets table

```sql
CREATE TABLE secrets (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
)
```

Values are stored in plain text — same protection level as the rest of the
SQLite database. Do not commit the DB file.

---

## Security notes

- `list_keys()` never returns values — safe to log or surface to the agent.
- `get()` and `set()` return/accept the raw value — never log these.
- Keys are case-sensitive uppercase by convention (`HUGGINGFACE_TOKEN`).
- **The `secrets/` folder is distinct from this store.** Some credentials live on disk under
  a cwd-relative `secrets/` directory (e.g. OAuth tokens written by MCP servers). The
  filesystem read tools (`read_file`, `grep_files`, `list_files`, `search_file`,
  `get_ast_outline`) are **denied** access to `secrets/` via seeded approval rules, so their
  contents never reach the LLM context. See [approval/index.md](approval/index.md). External
  MCP server processes read those token files directly and are unaffected.

---

## When to Update This File

- A new well-known secret key is introduced
- Access patterns change (new tool, new plugin using secrets)
- `secrets` table schema changes
