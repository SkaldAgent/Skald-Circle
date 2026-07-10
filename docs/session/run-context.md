# RunContext — Session Permissions & Configuration

**Single source of truth for RunContext.** Consolidates resolution, fields, usage, and API.

---

## Overview

Each session can have an active **RunContext** that controls:

- **Approval policy** — which permission group (`security_group`) applies to tool calls
- **System prompt injection** — dynamic prompt fragments per session
- **File-write pre-authorization** — paths auto-approved for writes (`allow_fs_writes`)
- **File-read auto-allow** — paths auto-approved for reads (working dir + `docs/` + `skills/` + `allow_fs_reads` + anything writable)
- **Working directory** — effective CWD for tool calls and file operations

`RunContext` is a JSON blob stored in the DB:
- `chat_sessions.run_context` — interactive web/mobile sessions
- `scheduled_jobs.run_context` — cron tasks
- `projects.run_context` — project-level defaults
- `project_tickets.run_context` — ticket-level overrides

---

## Fields

| Field | Type | Default | Purpose |
|-------|------|---------|---------|
| `security_group` | `Option<String>` | `null` | Permission group ID for approval rule lookup. Rules in this group take precedence over `"default"`. |
| `system_prompt` | `Vec<String>` | `[]` | Prompt fragments injected as dynamic system context every turn (joined with `"\n\n"`). |
| `allow_fs_writes` | `Vec<String>` | `[]` | Paths pre-authorized for file writes. Resolved against the working dir; recursive directory prefix match (no globs). Entries are also readable (write implies read). |
| `allow_fs_reads` | `Vec<String>` | `[]` | Extra **read-only** grants, beyond the working dir / `docs/` / `skills/` (always-safe baseline) and `allow_fs_writes`. Same prefix-match semantics. |
| `working_directory` | `Option<String>` | `null` | Effective WD for tool calls; `null` = Skald's process cwd. |

---

## Applicative Methods

`RunContext` exposes these methods (the session handler is agnostic to internal fields):

```rust
rc.tool_group_id()         -> Option<&str>   // for approval rule lookup
rc.extra_system_prompt()   -> Option<String>  // joins system_prompt with "\n\n"
rc.effective_working_dir() -> PathBuf         // configured path or process cwd
rc.is_write_allowed(path)  -> bool            // pre-auth check for file writes
rc.is_read_allowed(path)   -> bool            // pre-auth check for file reads
```

Both `is_*_allowed` canonicalize the path first (resolving `..` and symlinks via
`tools::fs::canonicalize_for_policy`) so traversal/symlink escapes cannot widen a grant.

---

## Resolution at Session Creation

**Order of precedence** (`ChatSessionManager::create_session` or `ChatHub::provision_session`):

1. **Explicit `run_context` parameter** — JSON blob passed at session creation. Persisted in DB immediately so the handler reads it at construction.
2. **Config-driven defaults** — from `config.yml` (per-source or per-agent), or TIC's `tic.run_context` key.
3. **None** — all RunContext methods return zero values (`tool_group_id()` → `None`, `is_write_allowed()` → `false`, `effective_working_dir()` → process cwd).

The `RunContext` is stored in `ChatSessionHandler::run_context` (`RwLock<Option<RunContext>>`). The handler **reads it once at construction** and **never directly accesses its internal fields** — only calls applicative methods.

### Session Handler Usage

| Method | Used for |
|--------|----------|
| `tool_group_id()` | Approval rule lookup (passed to `ApprovalManager::check()`) |
| `extra_system_prompt()` | Injected as dynamic system tail in `build_agent_config` (see [llm-loop.md](llm-loop.md)) |
| `is_write_allowed(path)` | Fast-path for file write tools; upgrades a `Require` decision to `Allow` |
| `is_read_allowed(path)` | Fast-path for file read tools; upgrades a `Require` decision to `Allow` |
| `effective_working_dir()` | WD injection for file tools and `execute_cmd` |

---

## Runtime Update

**Endpoint:** `POST /api/sessions/{id}/run_context`

**Body:** Full `RunContext` JSON (or `null` to clear):

```json
{
  "security_group": "cron_restrictive",
  "system_prompt": ["Always reply in English.", "Use metric units."],
  "allow_fs_writes": ["data/output", "logs/*"],
  "working_directory": "/projects/skald"
}
```

**Effects:**
- Updates `chat_sessions.run_context` in DB
- If the handler is live in memory, calls `handler.set_run_context()` immediately (no restart needed)
- Changes take effect on the next turn

---

## Approval Gate Integration

Rules are scoped to **permission groups** (`tool_permission_groups` table). A session's `RunContext.security_group` field references a group; rules in that group take precedence over `"default"`.

### Evaluation Chain

```
chat_session.run_context  (JSON blob)
  └─► RunContext.tool_group_id()  → e.g. "cron_restrictive"
        │
        ├─ rules WHERE group_id = "cron_restrictive"  ← evaluated first
        └─ rules WHERE group_id = "default"           ← fallback
```

**Default behavior:** If a session has no `run_context` or the blob has no `security_group`, only `"default"` group rules apply.

The `"default"` group is seeded automatically at startup and **cannot be deleted**. Its rules can be freely edited.

See [approval/index.md](../approval/index.md) for rule evaluation and pattern matching.

---

## Filesystem fast-paths and gate precedence

`RunContext` pre-authorizes filesystem access **without a human approval prompt**, but it
**never overrides an explicit `Deny`**. The gate in `llm_loop.rs` evaluates in this order:

```
1. ApprovalManager::check()          → Allow | Deny | Require
2. if Require and is_file_read_tool  → rc.is_read_allowed(path)  ? Allow : Require
   if Require and is_file_write_tool → rc.is_write_allowed(path) ? Allow : Require
```

So a `Deny` (e.g. the seeded `secrets/` rule) always wins, and the fast-path only relaxes
a `Require` to `Allow` — the same semantics as a session bypass. When the session has no
`RunContext`, a default one is used (working dir = process cwd), so `docs/`/`skills/` and the
cwd are still auto-readable.

### Writes — `allow_fs_writes`

Pre-authorizes writes to the listed paths. Resolved against the working dir; matching is a
recursive directory prefix (canonicalized — no globs). `memory/` is always writable via a
separate hardcoded exception in the approval engine.

**Common presets:** `["data"]` (project output), `["logs", "tmp"]` (temporary files).

### Reads — auto-allow roots

Reads are auto-allowed (no prompt) for, in order:

1. the **working directory** itself,
2. its **`docs/`** and **`skills/`** subtrees (always-safe baseline),
3. every **`allow_fs_reads`** entry (read-only grants),
4. everything in **`allow_fs_writes`** (write implies read).

Anything else falls back to the approval rules of the `security_group`. Under a
`require`-default group this means reads outside these roots prompt for approval.
The built-in **`default`** group is itself require-by-default (its final catch-all is
`require *`; see [approval/index.md](../approval/index.md)), so these auto-allow roots are
load-bearing even for the default group — not just for explicitly-restrictive custom groups.

> **`secrets/` is denied.** The approval engine seeds `deny` rules for the read tools on
> `secrets` / `secrets/*` (see [approval/index.md](../approval/index.md)). Because `Deny`
> is evaluated first and is non-bypassable, `secrets/` stays unreadable even though it sits
> inside the auto-read working dir. The recursive read tools (`grep_files`, `list_files`)
> additionally skip any `secrets` directory during traversal, so a search rooted higher up
> cannot leak secret values.

---

## Project Integration

Projects can set default `RunContext` for all interactive and ticket chats under that project:

- **Project-level:** `POST /api/projects/{id}/run_context` → stored in `projects.run_context`
- **Ticket override:** `POST /api/projects/{project_id}/tickets/{ticket_id}/run_context` → stored in `project_tickets.run_context`

Ticket override **takes precedence** over project-level when a ticket chat is opened.

See [projects.md](../projects.md) for project lifecycle and `build_runtime_run_context`.

---

## Example Scenarios

### Scenario 1: Cron job with restricted permissions

**Config:**
```json
{
  "security_group": "cron_restrictive",
  "allow_fs_writes": ["logs/*"],
  "working_directory": "/tmp"
}
```

**Effect:**
- All tool calls evaluated against `cron_restrictive` group rules (typically more restrictive)
- File writes to `logs/*` bypass approval entirely
- File operations use `/tmp` as working directory

### Scenario 2: Project ticket with context injection

**Config:**
```json
{
  "system_prompt": ["You are fixing ticket #42: DB migration failure. Current logs are in `logs/migration.log`."],
  "working_directory": "/projects/skald",
  "allow_fs_writes": ["data/migrations/", "logs/*"]
}
```

**Effect:**
- Extra context prepended to system prompt every turn
- Ticket-scoped working directory for `execute_cmd`
- Migration files and logs can be written without approval

### Scenario 3: Interactive session (default)

**Config:**
```json
{
  "security_group": null,
  "system_prompt": [],
  "allow_fs_writes": [],
  "working_directory": null
}
```

**Effect:**
- Default approval group rules apply
- No extra system prompt
- All file writes require approval (if rule triggers)
- Reads of the cwd, `docs/` and `skills/` are auto-allowed; `secrets/` is denied; other reads follow the default group rules
- Uses Skald's process CWD

---

## When to Update This File

- Adding a new field to `RunContext`
- Changing resolution order or approval gate behavior
- Adding new pre-authorization scenarios
- Documenting config best practices
