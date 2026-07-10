# Tools

## Tool Trait

```rust
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;            // JSON Schema object
    fn describe(&self, args, length) -> String { name }       // default impl — UI/notification label
    fn target_path(&self, _args: &Value) -> Option<String> { None }  // default impl — file this call opens, if any
    fn execute(&self, _args: Value) -> Result<String> { /* default: Err */ }
    fn execute_async<'a>(&'a self, args: Value) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
    fn run<'a>(&'a self, args: Value) -> Box<dyn ToolExecution + 'a> { /* default: SimpleExecution(execute_async) */ }
    fn category(&self) -> ToolCategory;              // access-control grouping
    fn sub_agents_only(&self) -> bool { false }      // default impl — visible only to sub-agents (depth > 0)
    fn root_agent_only(&self) -> bool { false }      // default impl — visible only to root agent (depth == 0)
    fn openai_definition(&self) -> Value { ... }     // default impl, rarely overridden
}
```

`Tool` is the **definition** (the catalogue entry in `ToolRegistry`); a single
live invocation is a `ToolExecution` produced by `run()`. See
[Tool execution lifecycle](#tool-execution-lifecycle) below.

**Two execution paths:**

- **Sync tools** implement `execute(&self, args)` only. The default `execute_async` wraps it in a ready future — no changes needed.
- **Async tools** (e.g. `image_generate`, `image_generate_providers_list`) implement `execute_async` directly and omit `execute`. Do NOT use `block_in_place` — override `execute_async` instead.

The dispatcher in `llm_loop.rs` drives every tool through `Tool::run(args) → ToolExecution`, so sync and async tools are dispatched uniformly (and cancellably).

---

## Tool execution lifecycle

`Tool` (definition) and `ToolExecution` (a single in-flight invocation) are split
on the `Command → spawn() → Child` pattern. `Tool::run(args)` starts one
execution and returns a handle that owns its state and implements its own stop.
Defined in [`crates/core-api/src/tool.rs`](../crates/core-api/src/tool.rs).

```rust
pub trait ToolExecution: Send + Sync {
    fn state(&self) -> ToolExecutionState;
    fn wait<'a>(&'a self) -> Pin<Box<dyn Future<Output = ExecutionOutcome> + Send + 'a>>;
    fn stop<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> { /* default: no-op */ }
}
```

**States** (`ToolExecutionState`, richer than the persisted status string):

| State | Meaning | DB `status` |
| --- | --- | --- |
| `Pending` | created, not yet started (transient, not persisted alone) | `running` |
| `AwaitingApproval` | blocked on a human approve/clarification | `pending` |
| `Running` | actively executing | `running` |
| `Completed` | finished OK | `done` |
| `Failed` | tool/runtime error | `failed` |
| `Cancelled` | stopped by `/stop` — **not** an error | `cancelled` |
| `Rejected` | denied by policy/human — **not** an error | `rejected` |

`Cancelled` and `Rejected` are deliberately distinct from `Failed` so a stop or a
policy denial never pollutes error metrics. `wait()` only ever returns the three
terminal `ExecutionOutcome`s (`Completed` / `Failed` / `Cancelled`); the
approval-phase states are owned by the session driver.

**Purity.** A `ToolExecution` never touches the DB or the WebSocket. The session
driver (`ChatSessionHandler`) mirrors its state to `chat_llm_tools` and emits the
`ToolStart`/`ToolDone`/`ToolError`/`ToolCancelled`/`ToolRejected` events.

**`SimpleExecution`** is the default handle: a work future + a stop-token. `wait`
races the two, so `stop()` (or the driver dropping `wait`) drops the work future,
aborting the in-flight I/O — enough to make `/stop` responsive for **every**
I/O-bound tool with zero per-tool code (this includes `execute_cmd`, whose
`kill_on_drop(true)` child dies when the future is dropped).

**`drive_execution(exec, cancel_token)`** is the generic driver: it runs `wait`
and, when the turn's `/stop` token fires, calls `exec.stop()` once. Used by both
the live loop (`llm_loop.rs`) and `resume_pending_tools` (`resume.rs`).

**Bespoke `stop()` (extension point).** Tools that must tear down *remote* work
override `run()` to return their own `ToolExecution` whose `stop()` does more than
drop the future — e.g. a ComfyUI image tool POSTing `/interrupt` so the server
stops generating too. Dropping the future already frees the client; a bespoke
`stop()` propagates the cancellation to the far side. (Not yet wired for any
built-in tool — the default covers responsive `/stop` today.)

**Rehydration = re-run from intent.** A persisted tool call is `(name, args, status)`;
the live future is never serialized. `resume_pending_tools` reconstructs an
execution via `build_execution(name, args)` and re-runs it from the start — it
does not resume a checkpoint.

**`sub_agents_only`**: if a tool returns `true`, it is excluded from the root agent's tool list and only added to sub-agent configs (depth ≥ 1) in `dispatch_sub_agent`. Default is `false`.

**`root_agent_only`**: if a tool returns `true`, it is included in the root agent's tool list but filtered out from sub-agent configs in `AgentRunConfig::for_sub_agent()`. Default is `false`.

Both flags are mutually exclusive — a tool should never return `true` for both. If it does, it will be invisible to all agents.

---

## ToolCategory

Every tool declares a `ToolCategory`, used for access-control filtering and audit:

| Variant | Used by |
| --- | --- |
| `Filesystem` | File read/write tools (`read_file`, `write_file`, `edit_file`, …) |
| `Shell` | `execute_cmd`, `restart` |
| `Subagent` | `execute_task` / `execute_subtask` (synthetic — not in registry) |
| `Introspection` | `list_items`, `image_generate_providers_list` |
| `Config` | `register_mcp`, `delete_mcp`, `toggle_item`, `execute_task` (InterfaceTool, interactive only), `delete_cron_job`, `configure_plugin`, `image_generate`, `set_secret`, `list_secrets` |

---

## ToolRegistry

`HashMap<String, Arc<dyn Tool>>` with four public methods:

| Method | Purpose |
| --- | --- |
| `register(tool)` | Insert tool keyed by `tool.name()` |
| `openai_definitions()` | Returns definitions for root-agent tools (excludes `sub_agents_only`) |
| `openai_definitions_sub_agents_only()` | Returns definitions for tools where `sub_agents_only() == true` |
| `root_agent_only_names()` | Returns names of all tools where `root_agent_only() == true` — used by `for_sub_agent()` to filter |
| `list_all()` | Returns `(name, description)` for all registered tools (sorted) |
| `category_of(name)` | Returns `Option<ToolCategory>` for a registered tool; `None` for MCP/interface/unknown tools |
| `dispatch(name, args)` | Executes tool by name to a `Result<String>`; errors on unknown name (used by the REST resolve endpoint) |
| `run(name, args)` | Starts a `ToolExecution` for a registered tool; `None` for unknown names (MCP/interface handled by the caller). The cancellable dispatch path. |
| `describe_call(name, args, length)` | Returns a human-readable label for any tool call (including non-registry tools). Falls back to `name` for unknown tools. |

---

## ToolCatalog

`ToolCatalog` (`src/core/tool_catalog.rs`) is a unified façade wrapping `ToolRegistry` + `McpManager`:

| Method | Purpose |
| --- | --- |
| `list_all() -> AllTools` | Returns all built-in tools (registry), a small **static list** of core tools injected outside the registry (`synthetic_tools()`: `execute_task`, `execute_subtask`, `update_scratchpad`, `ask_user_clarification`, `write_todos`, `activate_tools`, `notify`, `show_file_to_user`, `image_generate`), and MCP tools as a single `AllTools { built_in, mcp }` struct. Used by `GET /api/approval/tools`. |
| `describe_call(name, args, length) -> String` | Pass-through to `ToolRegistry::describe_call()`. |

`AllTools` and `ToolInfo` are `#[derive(Serialize)]` — the frontend handler can return `Json<AllTools>` directly.

### Dynamically-injected tools & discovery

Many tools reach the LLM **outside** the `ToolRegistry` — `InterfaceTool` closures
(`write_todos`, `notify`, `show_file_to_user`, `activate_tools`), plugin tools
(Telegram `send_voice_message`/`send_attachment`, honcho `memory_*`) and provider
tools (`image_generate`). The `synthetic_tools()` static list above eagerly
surfaces the *core-owned* ones so they can be pre-configured in the Security-groups
UI before first use; plugin/provider names are intentionally left out so core
stays decoupled from them.

To cover the rest without a hand-maintained mirror, [`ToolDiscovery`](../src/core/tool_discovery.rs)
taps the single point where the tool array is assembled — `AgentRunConfig::all_tool_defs()`,
observed each round from `llm_loop.rs` — and upserts every offered tool into the
`known_tools` table (in-memory seen-set guard → DB write only for new names, off
the turn's critical path). `GET /api/approval/tools` merges `known_tools` into the
response so any tool that has been offered at least once becomes gate-able. This
**cannot drift** from what is really offered and needs no per-tool/per-plugin
wiring. See [approval docs](approval/index.md#alltools-response-get-apiapprovaltools).

---

## Tool Name Constants

All system tool names are centralised in `src/core/tools/tool_names.rs` as `pub const` strings. Import with `use crate::tools::tool_names as tn;`.

| Constant | Value |
| --- | --- |
| `tn::EXECUTE_TASK` | `"execute_task"` |
| `tn::EXECUTE_SUBTASK` | `"execute_subtask"` |
| `tn::RESTART` | `"restart"` |
| `tn::UPDATE_SCRATCHPAD` | `"update_scratchpad"` |
| `tn::WRITE_TODOS` | `"write_todos"` |
| `tn::ASK_USER_CLARIFICATION` | `"ask_user_clarification"` |
| `tn::ACTIVATE_TOOLS` | `"activate_tools"` |
| `tn::NOTIFY` | `"notify"` |
| `tn::READ_NOTIFICATION` | `"read_notification"` |
| `tn::EXECUTE_CMD` | `"execute_cmd"` |
| `tn::SHOW_FILE_TO_USER` | `"show_file_to_user"` |

**Rule:** never hardcode these strings in new code — always use the constants. This ensures that a rename is a single-file change and that typos produce a compile error rather than a silent dispatch miss.

---

## Registration Pattern

All tools are registered in `src/main.rs` before `ChatSessionManager` is built.

**Not in ToolRegistry — synthetic tools intercepted in `run_agent_turn`:**

- `execute_task` — interactive-session sub-agent/task launcher (modes: `cron`, `sync`=inline sub-agent, `async`=background). Intercepted in `run_agent_turn` only for `mode=sync`; `cron`/`async` go through the normal cancellable path as InterfaceTools
- `execute_subtask` — background-session sub-agent launcher (sync-only). Injected as an InterfaceTool in cron/async sessions in place of `execute_task`
- `update_scratchpad` — writes to `session_scratchpad` table; **shared** blackboard injected into every agent in the session; available to all agents
- `write_todos` — **stateless** private task list (TodoWrite-style: the agent re-sends the whole list with statuses on every call); available to all agents. Unlike the scratchpad it is **not** persisted and **not** shared: the formatted checklist lives only in the calling agent's own tool-result history, which is per-stack, so sub-agents and the caller never see it. Handled by `dispatch_write_todos` (`agent_dispatch.rs`); no DB table involved
- `ask_user_clarification` — pauses and asks the user a question; available to every agent except hidden `system` agents (e.g. TIC). Routing depends on session type:
  - **Interactive sessions** (web, Telegram): available at any depth (root `chat` agent included); emits `ServerEvent::AgentQuestion` inline (and registers with `ClarificationManager`), so the user can answer either from the chat or the Agent Inbox
  - **Background sessions** (cron): available at any depth; registers with `ClarificationManager` only, visible in Agent Inbox; agent suspends until answered
- `activate_tools` — activates **tool groups** on demand (lazy loading): MCP server names and/or the reserved `config` group (the built-in `Config`-category tools). Injected as an `InterfaceTool` in `build_agent_config` (root, session-scoped) and in `dispatch_sub_agent` (sub-agents, stack-scoped). See [Lazy `config` tool group](#lazy-config-tool-group)
- `notify` — queues a structured notification (`Notification`: `{source, event_type, summary, event_time, refs}`) to the home conversation via `ChatHub`; **injected as an `InterfaceTool` by the caller** (`TicManager` for TIC, `TaskManager` for background task agents); not in ToolRegistry so ordinary agents cannot call it
- `show_file_to_user` — opens a file in the user's UI. Emits `ServerEvent::OpenFile`, which the SPA routes to the [file viewer](frontend.md#file-viewer) (every kind, HTML included). Supported formats in the file viewer: Markdown, source code, plain text, raster images (PNG/JPG/GIF/WebP/…), SVG, PDF, LaTeX (`.tex` / `.latex` — compiled to PDF automatically on the server), and HTML (`.html` / `.htm` — rendered live in an origin-isolated `<iframe>`, toggleable to source). **For a LaTeX document the agent should pass the `.tex` source, not a pre-built `.pdf`**: only the `.tex` path triggers server-side compilation, dependency-aware caching, and dependency watching (see [LaTeX compile & cache](frontend.md#latex-compile--cache)). A raw `.pdf` is served statically — never recompiled, dependencies not watched — so editing a source fragment leaves the rendered `.pdf` stale. The tool description enforces this guidance. **Injected as an `InterfaceTool` only for SPA clients** in `ws.rs` (sources `web` + `mobile`); the Telegram plugin goes through a separate handler and never receives it — its analogue is `send_attachment`. Built by `tools::show_file::make_tool(hub, source)`; the path emitted to the frontend is normalised by `fs::relativize_for_display` (relative to the project root when inside it, absolute otherwise)

**Also not in ToolRegistry:**

- MCP tools — injected dynamically per-request via `McpManager::tools()`

---

## Tool Visibility Filtering (permission groups)

Tools are filtered out of the LLM's tool list when the effective approval rule for the session's **permission group** marks them `Deny`. The group comes from the session's run context (or the built-in `"default"` group). This replaces the removed per-agent `allow_tools` whitelist — see [approval/index.md](approval/index.md).

**MCP tools are never filtered here** — they pass through regardless of the group. The Approval gate governs MCP tool execution.

Filtering happens in `src/core/session/handler/config.rs` (depth 0) and `agent_dispatch.rs` (sub-agents) after assembling `base_tool_defs` (registry + synthetic tools), before extending with MCP tools. The same interactive-only and approval-visibility filters are applied to `config_tool_defs` (the lazy `config` group), so activating `config` never bypasses a group's `Deny` rules.

---

## Built-in Tool Catalogue

| Tool name | Module | Category | Approval | Sub-agents only |
| --- | --- | --- | --- | --- |
| `list_files` | `tools::fs` | Filesystem | No | No |
| `read_file` | `tools::fs` | Filesystem | No | No |
| `write_file` | `tools::fs` | Filesystem | Yes (non-memory/) | No |
| `edit_file` | `tools::fs` | Filesystem | Yes (non-memory/) | No |
| `insert_at_line` | `tools::fs` | Filesystem | Yes (non-memory/) | No |
| `replace_lines` | `tools::fs` | Filesystem | Yes (non-memory/) | No |
| `search_file` | `tools::fs` | Filesystem | No | No |
| `grep_files` | `tools::fs` | Filesystem | No | No |
| `get_ast_outline` | `tools::ast_outline` | Filesystem | No | No |
| `execute_cmd` | `tools::exec` | Shell | **Always** | No |
| `restart` | `tools::restart` | Shell | **Always** | No |
| `list_items` | `tools::list_items` | Introspection | No | Merged listing for `type` ∈ {mcp, plugins, cron, agents}. For `agents`, each entry carries `id`, `name`, `description`, optional `instructions` (how to call the agent well, present only when set in `meta.json`), and optional `client`. |
| `register_mcp` | `tools::register_mcp` | Config | No | No |
| `delete_mcp` | `tools::register_mcp` | Config | No | Permanently unregisters + disconnects an MCP server (destructive; kept separate from `toggle_item` like `delete_cron_job`) |
| `toggle_item` | `tools::toggle_item` | Config | No | Merged enable/disable for `kind` ∈ {mcp, plugin, cron} |
| `execute_task` | InterfaceTool (not in registry) | Config | No | Interactive sessions only; `session_id` and `run_context_id` captured in closure at tool-build time; tasks inherit the parent RunContext |
| `execute_subtask` | InterfaceTool (injected in run_job) | — | No | Background sessions only (sync sub-tasks); inherits `run_context_id` from the parent job |
| `read_agent_result` | synthetic | — | No | Interactive only; always returns not_ready; real delivery is async synthetic message |
| `delete_cron_job` | `tools::cron_jobs` | Config | No | No |
| `configure_plugin` | `tools::configure_plugin` | Config | No | No |
| `set_secret` | `tools::set_secret` | Config | No | No |
| `list_secrets` | `tools::list_secrets` | Config | No | No |
| `read_notification` | `tools::read_notification` | Introspection | No | Root only (depth == 0) |
| `image_generate_providers_list` | `tools::image_generate` | Introspection | No | No |
| `image_generate` | `tools::image_generate` | Config | No | No |
| `update_scratchpad` | synthetic | — | No | No |
| `write_todos` | synthetic (stateless) | — | No | No — private per-stack list; not shared with sub-agents or caller |
| `ask_user_clarification` | synthetic | — | No | No — all agents except `system` (TIC) |
| `activate_tools` | synthetic (per-session) | Config | No | No |

---

### Lazy `config` tool group

The built-in **`Config`-category** registry tools are **not** part of the always-on tool set. Like MCP tools, they are lazy-loaded on demand: the LLM calls `activate_tools(["config"])` to bring them into context from the next round onward.

- **Affected tools** (registered in `ToolRegistry` with `ToolCategory::Config`): `set_secret`, `list_secrets`, `register_mcp`, `delete_mcp`, `configure_plugin`, `delete_cron_job`, `toggle_item`.
- **Mechanism**: `build_agent_config` builds the base set with `ToolRegistry::openai_definitions_excluding_config()` and carries the config defs separately as `AgentRunConfig.config_tool_defs` (from `openai_definitions_config_only()`). `all_tool_defs()` appends them only when `active_mcp_grants` contains the reserved `"config"` string. The grant persists in `session_mcp_grants` / `stack_mcp_grants` exactly like an MCP server name. See [mcp.md → Config tool group](mcp.md#config-tool-group).
- **Not affected**: `image_generate` / `image_generate_providers_list` come from the image-generator manager (not the `ToolRegistry`), so despite `image_generate`'s `Config` category it stays always-on. `list_items` is `Introspection`, so listing MCP/plugins/cron stays available without activating `config`; only the mutating `toggle_item` is gated.
- **Discoverability**: a short static hint lives in `agents/main/AGENT.md` and `agents/project-coordinator/AGENT.md`. The `<!-- MCP_LIST -->` block stays MCP-only.

---

### Key Parameter Notes (recent additions)

| Tool | New parameters | Notes |
| --- | --- | --- |
| `execute_cmd` | `workdir` (absolute path), `timeout` (1–600 s, default 120) | Output truncated at 100 KB. Description tells LLM to use dedicated tools (`read_file`, `grep_files`, etc.) instead of shell equivalents. **Audit:** every command is logged at `info` (`execute_cmd: running shell command`, fields `command`/`workdir`/`timeout_secs`) before running, so auto-approved commands (approval bypass active) are still traceable. |
| `edit_file` | `replace_all` (bool, default false) | Replaces every occurrence when true; otherwise requires unique match. Description tells LLM to use instead of `sed`/`awk`. |
| `grep_files` | `output_mode` (`content`/`files_only`/`count`), `context_lines` (0–10), `offset` (pagination) | Description tells LLM to use instead of `grep`/`rg`. Result paths are relative to the queried directory (stripped of the resolved root), consistent with `list_files`. |
| `get_ast_outline` | `path` | Returns top-level definitions (functions, classes, structs, methods) without bodies. **tree-sitter 0.26** backend for: `.py .js .mjs .ts .tsx .go .java .c .h .cpp .cc .hpp .swift .lua .rb .sh .ex .exs .json .yaml .yml .html .css`. **syn** backend for `.rs`. Text/regex fallback for `.kt .toml .md .sql` (grammar crates incompatible with tree-sitter 0.26 at time of writing). |

---

## Tool Display Labels

Every `Tool` implementation can override `describe(&self, args: &Value, length: ToolDescriptionLength) -> String` to produce a compact human-readable label shown in the UI and on Telegram instead of the raw tool name.

| Length | Max chars | Example |
| --- | --- | --- |
| `Short` | 60 | `execute_cmd \`git\`` |
| `Full` | 120 | `execute_cmd \`git commit -m "feat: ..."\`` |

Constants `MAX_LABEL_SHORT` and `MAX_LABEL_FULL` are defined in `src/core/tools/mod.rs`. `truncate_label(s, max)` truncates at char boundary appending `…`.

The default implementation returns `self.name()`, so all tools work without implementing `describe`. Built-in tools (fs, exec) have explicit implementations; MCP and plugin tools fall back to the tool name.

`ToolRegistry::describe_call(name, args, length)` is the single call-site used by `llm_loop.rs`, `resume.rs`, and the `/api/{source}/messages` history endpoint. It also handles synthetic tools (`call_agent`) that are not in the registry.

Labels are emitted in `ServerEvent::ToolStart` as `label_short` and `label_full` and included in history responses so the frontend always has them.

## Clickable Target Path

A tool can override `target_path(&self, args: &Value) -> Option<String>` to advertise a **single viewable file** the call acts on. The single-file fs tools (`read_file`, `write_file`, `edit_file`, `insert_at_line`, `replace_lines`, `search_file`) return their `path` argument via the `fs::path_arg` helper; directory tools (`list_files`, `grep_files`) and every other tool return `None` (the default).

`ToolRegistry::target_path(name, args)` is the registry-level accessor, mirroring `describe_call`. Its result is emitted in `ServerEvent::ToolStart` as the optional `path` field (omitted when `None`) and included in `/api/{source}/messages` history items. The frontend renders that path as a link that opens the [file viewer](frontend.md#file-viewer) via `window.openFile(path)`.

`show_file_to_user` is an `InterfaceTool` (not in the registry, so it has no `Tool::target_path`). Because its whole purpose is to open a file, both `describe_call` and `target_path` special-case it **inline**: the label becomes `` show_file_to_user `path` `` and the same raw `path` arg is returned, so the frontend's `renderLabel` makes the path in the label clickable with no extra wiring.

The sub-agent delegation tools — `execute_task`, `execute_subtask`, and the legacy `run_subtask` alias — are likewise `InterfaceTool`s outside the registry, so they have no `Tool::describe`. `describe_call` special-cases them **inline** via `describe_sub_agent_call`, building a label from the call's `agent_id` + `description` (falling back to `title`, then to the bare name) so the UI/Telegram shows what is being delegated instead of the raw tool name. For `execute_task` the `mode` is shown as a single compact emoji appended to the tool name: sync → ⚡, async → 🚀, cron → 📅. Example: `execute_task ⚡ → software-engineer: Refactor auth module`.

---

## FS Path Resolution

`tools::fs::resolve(path)`:

- If path starts with `/` → used as absolute path
- Otherwise → resolved relative to CWD (project root when running via `run.sh`)

Paths starting with `memory/` bypass the approval gate for write tools.

### Security-aware canonicalization

`tools::fs::canonicalize_for_policy(path, base)` resolves a path to its canonical absolute
form (resolving `..` and symlinks of the longest existing ancestor) for security
prefix-matching. It is shared by the RunContext fast-paths (`is_write_allowed` /
`is_read_allowed`) and `approval::normalize_path`, so traversal/symlink tricks like
`docs/../secrets/x` cannot evade an allow grant or a deny rule. `path_under(child, base)`
does the component-wise prefix test.

### Read auto-allow & `secrets/` deny

Read tools (`read_file`, `grep_files`, `list_files`, `search_file`, `get_ast_outline`) are
auto-allowed without a prompt when the path is under the working dir, `docs/`, `skills/`,
`allow_fs_reads`, or `allow_fs_writes` (the RunContext read fast-path). The `secrets/`
directory is denied for these tools via seeded `deny` rules; the recursive ones
(`grep_files`, `list_files`) additionally list `secrets` in their `SKIP_DIRS` so a search
rooted higher up never descends into it. See [approval/index.md](approval/index.md) and
[session/run-context.md](session/run-context.md).

---

## Adding a Tool

1. Create a struct in `src/core/tools/` (new file or existing module).
2. `impl Tool` for the struct — include `fn category()`.
3. Register in `src/main.rs`: `tool_registry.register(MyTool::new(...))`.
4. If the tool should only be visible to certain agent depths, implement `sub_agents_only()` or `root_agent_only()` instead of using `InterfaceTool` injection.
5. If the tool needs `ChatHub`, a per-session resource, or should only be visible to specific callers, do **not** add it to `ToolRegistry` — implement it as an `InterfaceTool` and inject it at the call site (see `tools::notify::make_tool`).
6. If the tool needs user approval before executing, add an `approval_rules` row (or let the admin add one). The approval gate (`ApprovalManager::check`) is rule-driven — no code change required unless the default-open policy is not suitable.
7. Update this doc (catalogue table).

---

## When to Update This File

- A tool is added, removed, or renamed
- The approval rules for a tool change
- The `Tool` trait gains or loses a method
- `ToolCategory` gains a new variant
- The tool visibility (permission-group) filtering logic changes
