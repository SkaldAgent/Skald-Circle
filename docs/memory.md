# Memory System

`src/core/memory/mod.rs`

---

## Purpose

Provides a pluggable long-term memory layer for the LLM. Before every user turn,
the active memory backend is queried for relevant context and the result is
injected into the system prompt. Memory backends can also expose optional LLM
tools (e.g. `memory_query`).

The file-based MD memory (`data/memory/`) continues to exist in parallel and is
managed autonomously by the LLM via the `read_file` / `write_file` tools — it is
not part of this system.

---

## Memory Trait

```rust
#[async_trait]
pub trait Memory: Send + Sync {
    fn id(&self) -> &str;
    fn is_available(&self) -> bool;
    async fn query_context(session_id: i64, user_message: &str) -> Option<String>;
    fn tools(&self) -> Vec<Arc<dyn Tool>> { vec![] }   // default: no tools
}
```

| Method | Description |
| --- | --- |
| `id()` | Unique backend identifier (e.g. `"honcho"`) |
| `is_available()` | `true` when the backend is up and ready. The manager skips unavailable backends silently. |
| `query_context()` | Returns a formatted string to inject into the system prompt, or `None` if nothing relevant is available (cold start, backend down, etc.) |
| `tools()` | Optional LLM-callable tools exposed by this backend. Called per turn. |

---

## MemoryManager

`Skald::memory_manager: Arc<MemoryManager>`

Holds **at most one** active backend.

### Singleton rule

| Situation | Result |
| --- | --- |
| No backend registered | New backend accepted, logged at `INFO` |
| Same id re-registers (plugin restart) | Old entry replaced, logged at `INFO` |
| Different id tries to register | Rejected with `error!`; existing backend kept |

### Methods

| Method | Description |
| --- | --- |
| `register(Arc<dyn Memory>)` | Register (or replace) a backend |
| `query_context(session_id, msg)` | Delegates to the backend if available; returns `None` otherwise |
| `tools()` | Returns backend tools if available; empty `Vec` otherwise |
| `tool_defs()` | OpenAI-format JSON definitions of the backend's tools |

---

## Integration in the LLM loop

### Context injection (read path)

In `ChatSessionHandler::handle_message`, **before** `build_agent_config`:

```
memory_manager.query_context(session_id, user_message)
  → Some(ctx) → prepended to extra_system_context
  → None      → extra_system_context unchanged
```

Called for **all sessions** — interactive, cron, and tic alike. Automated agents
benefit from knowing user preferences and context just as much as interactive
ones (e.g. a cron agent that knows "Daniele prefers Italian" produces better
output). Only the **write path** filters by `is_interactive`/`is_ephemeral`.

### Tool dispatch (per turn)

`build_agent_config` calls `memory_manager.tools()` and stores the result in
`AgentRunConfig::memory_tools`. These tools are:

1. Added to the LLM's tool list via `all_tool_defs()` (after base + MCP tools,
   before interface tools).
2. Dispatched in `run_agent_turn` before the global `ToolRegistry` fallthrough.
3. Inherited by sub-agents via `AgentRunConfig::for_sub_agent`.

---

## Adding a Memory Backend

1. Implement `Memory` on a struct (usually inside a plugin module).
2. In the plugin's `Plugin` trait impl, override `fn memory() -> Option<Arc<dyn Memory>>` to return `Some(...)`.
3. `PluginManager` calls `state.memory_manager.register(plugin.memory())` automatically after each successful `start()` / `reload()`.

No changes to `main.rs` or the session handler are needed.

---

## Current Backends

| Backend | Source | Plugin |
| --- | --- | --- |
| `honcho` | `src/core/plugin/honcho/mod.rs` | [honcho.md](honcho.md) |

---

## When to Update This File

- `Memory` trait methods change
- `MemoryManager` singleton rules change
- A new backend is added or removed
- Integration points in the session handler change
