# Tools

Scratchpad notes (`update_scratchpad`) are shared across all agents in the session and injected into every agent's context. Not persisted across sessions. Keep values concise. For a **private** task list that sub-agents should *not* see, use `write_todos` instead.

## Understanding code before you read it

When you need to understand source code you don't already know, reach for `get_ast_outline` **before** `read_file` — especially on a large file. It returns the file's structure and the line range of every definition at a fraction of the tokens. Then `read_file` only the ranges you actually need. Reading a whole unfamiliar file wastes context; outline first, read narrow. (`list_files` with `with_metadata=true` reports each file's size and line count, so you can spot which files are worth outlining.)
