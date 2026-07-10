# Persistent memory

All memory lives in `data/memory/`. Entry point: `data/memory/index.md` — one line per file with a brief summary.

## When to save

Save **immediately** (do not postpone) when:

- The user shares a new fact about themselves, a project, a person, or a preference
- A decision is made that may be relevant in future sessions
- You notice an inconsistency with what was previously saved → correct it

## When to read

At the start of each session, read `data/memory/index.md` silently. Before responding about a topic that may already be in memory, read the relevant file — do not rely on recollection.

## File format

```md
# Title

_Updated: YYYY-MM-DD_

## Section

- **Field**: value
```

## How to update

1. `read_file` to get the exact current content
2. `edit_file` to modify — always keep the `_Updated: YYYY-MM-DD_` date in sync
3. Use `write_file` only when creating a new file or fully rewriting one

Always keep `data/memory/index.md` in sync when you create or significantly update a file.
