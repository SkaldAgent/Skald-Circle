# Skills

A skill is a **folder of instructions and resources** that you load on demand, when a task calls for it — a procedure written once and followed every time, instead of re-deriving the steps in each conversation. A skill adds no tools and starts no processes: it is knowledge you read, then apply with the tools you already have.

## Where skills live

Skills are installed in one of two read-only trees:

| Path | Whose | Who installs |
| --- | --- | --- |
| `skills/shared/<id>/` | the whole group — every member sees these | an admin |
| `skills/<username>/` | one member's own | that member |

Both trees are **read-only**, everywhere and for everyone. You cannot create or edit files under `skills/` — not with the file tools, not from the shell, not even with `sudo`. A skill is **installed**, never written in place; the only way in is `skill_register` (below). To modify a skill you copy it out, edit the copy, and register it again.

## Using a skill

Your prompt already carries the index of the skills you can see: one line each, with the full path of its `SKILL.md` and a short description of when to use it. When a task matches — even partially — **read the skill before doing the work**: `read_file skills/<scope>/<id>/SKILL.md`, then follow its instructions.

To run a skill's script, use `execute_cmd` with `workdir` set to the skill's folder (e.g. `workdir: "skills/shared/ics-import"`). A skill cannot write next to itself — the tree is read-only — so scripts must write their output, caches and state to your home (`~`) or `/tmp`, never into the skill folder.

To see exactly what is installed, with full descriptions and per-skill health: `list_items` with `type="skills"`.

## Creating a skill — the authoring contract

A skill folder looks like this:

```
<skill-name>/
├── SKILL.md              # required
├── scripts/ or *.py, *.js in the root   # optional executables
├── references/           # optional documents to read on demand
└── assets/               # optional templates, examples
```

`SKILL.md` opens with a YAML frontmatter block, then the instructions in Markdown:

```markdown
---
name: ics-import
description: Download an iCalendar (ICS) feed and turn it into JSON or a table. Use whenever the user gives you a calendar URL or asks to import, inspect or summarize events from an ICS link.
---

# ICS import

1. Run `python3 scripts/ics2json.py <url>` from this folder...
```

Rules — every one of them is **checked at installation**, and a folder that breaks one is refused with a message naming the problem:

- **`name`**: lowercase letters, digits and hyphens only, at most 64 characters. It becomes the installed folder's name — so the working copy may be called `draft-2`, but what gets installed is named after the frontmatter.
- **`description`**: required, at most 1000 characters. This is the *use condition* — the only thing the model sees when deciding whether the skill is relevant. Write it assertively: say exactly **when** to reach for the skill, and err on the side of triggering too easily rather than too rarely. Save the detail for the body.
- **Write it in English** — the instructions, the frontmatter, the comments. Everything the model reads is English.
- **Self-contained**: no symbolic links; at most 500 files and 8 MiB in total. A skill holds instructions, scripts and reference documents — bulk data belongs in a home or a project.
- Create the folder **somewhere you can write** — your home, a project or a shared folder. **Not in `/tmp`** or anywhere else in the container-only filesystem: installation copies the folder from the host side and cannot see those paths.
- A script that sticks to the Python/Node standard library and the preinstalled command-line tools works always. One that needs PyPI or npm packages **must say so in the body** — those packages are installed by hand (`sudo pip install …`) and are lost when the container is recreated.

## Installing, updating, deleting

All three go through tools, and all three ask a human for approval first — the approval card shows the full `SKILL.md`, the file list and where the skill is going:

- **Install**: `skill_register(scope, path)` — `scope` is `"mine"` (your own tree) or `"global"` (everyone's; requires the admin capability). The folder is validated, copied in one atomic move, and appears in the prompt index immediately.
- **Update**: register again with the same `name` in the same scope — the old copy is replaced. That is the only way a skill changes.
- **Promote to the group**: register an already-installed skill with `scope: "global"`, e.g. `skill_register("global", "skills/maria/ics-import")`.
- **Delete**: `skill_delete(scope, id)` — the folder is removed, with no recycle bin. Use `list_items` with `type="skills"` to find ids.

## Getting a skill from a public repository

`fetch_repo(url, sub_path, destination)` downloads a subtree of a **public git repository** into a writable folder of yours — shallow, without the `.git` history. It installs nothing: if what you downloaded is a skill, review the files and then call `skill_register` on the destination folder. Every download leaves a `.source.json` ticket in the destination recording the URL, the sub-path and the exact commit it came from, so "where did this come from?" always has an answer.
