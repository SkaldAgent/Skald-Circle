# Session & Message Handling

Session management and the LLM message loop.

## Files

- [run-context.md](run-context.md) — RunContext: permissions, system prompt, file authorization (single source of truth)
- [session.md](../session.md) — ChatSessionHandler lifecycle, message flow, tool dispatch
- [llm-loop.md](llm-loop.md) — (TODO: split from session.md) Core LLM loop: handle_message, resume_turn, run_agent_turn

See [../index.md#session--llm-loop](../index.md#session--llm-loop) for navigation to related files (compaction, memory, event bus).
