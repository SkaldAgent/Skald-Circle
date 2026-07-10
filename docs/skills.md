# Skills System

Skills are reusable capability packages that extend what the agent can do without modifying the core source code.

## Structure

```
skills/
  index.md              ← registry of all available skills
  <skill-name>/
    SKILL.md            ← documentation: purpose, usage, script API
    <script>.py         ← one or more Python scripts
```

## How the agent uses skills

1. `skills/index.md` is injected into the agent's system prompt automatically as a
   `<skills_index>` block, so every agent discovers available skills without reading it
   explicitly. Controlled by the `inject_skills` meta.json flag (default `true`; see
   [agents.md](agents.md)) — set it to `false` for background agents that don't need skills.
   The block is skipped silently when no skills are installed.
2. It reads the relevant `SKILL.md` to understand how to invoke the script.
3. It runs the script via a shell command (e.g. `python3 skills/pdf/scripts/...`).
4. It uses the script's stdout as the result.

> **Path note:** the injected `<skills_index>` shows the index path relative to the session's
> working directory when it lives under it, absolute otherwise. In a project chat (working
> directory = project root) it shows as absolute, since skills live under Skald's own cwd.
> Invoking a skill from a project session still needs care — `execute_cmd` runs with the project
> working directory, so scripts referenced cwd-relative (`python3 skills/...`) won't resolve; use
> an absolute path or `cd` to Skald's root.

## Adding a skill

1. Create `skills/<name>/SKILL.md` — document purpose, required inputs, expected output, and example invocation.
2. Add the Python script(s) alongside it.
3. Register the skill in `skills/index.md` by adding a row to the table.

No code changes or restarts are required — the agent discovers skills at runtime by reading the index.

## Conventions

- Scripts must be runnable with `python3` and accept arguments from the command line.
- Scripts should write their result to stdout and errors to stderr, exiting with code `0` on success.
- Keep each script focused on one task. Compose multiple skills via the agent, not within a single script.
