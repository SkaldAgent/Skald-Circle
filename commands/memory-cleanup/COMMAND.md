You have been invoked with /memory-cleanup.

Your task is to restore `data/memory/` to a clean, well-structured state: repair the index, remove dead and stale references, surface orphan files, split a bloated index, and rebalance the directory tree when it has grown disorganised. You do not know the user — learn the language and conventions from the files themselves.

## Scope

**Structural integrity only.** Your unit of work is *references and paths*, not the merit of each note. You are not doing a per-file content review, not writing a profile summary, and not editing note bodies. The goal is an index the user can trust and a tree that is easy to navigate.

## Workflow

### 1. Discover conventions
Read the root `data/memory/index.md` and skim 1–2 subdirectory indexes if present. Infer and record:
- **Language** — match it for all replies and for any text you write into files.
- **Header block** — whatever sits at the top of an index before the link sections (e.g. `_Updated:` date, a profile/about line, a title). **Preserve it byte-for-byte**; only bump the `_Updated:` date at the very end.
- **Section style** — how sections are headed (emoji + title? plain markdown `##`?), how links are listed (bullet with a `—` gloss?). Mirror this style in any index you create or edit.
- **Naming convention** — kebab-case, snake_case, etc. Note any drift between files.
- **Subdirectory convention** — do subdirectories carry their own `index.md`? Follow the same pattern for new ones.

### 2. Integrity scan
Scan every `.md` file under `data/memory/` (not just indexes). Report findings grouped by severity. Use compact tables.

**A. Dead links** — every internal Markdown link (relative path resolving inside `data/memory/`) whose target file does not exist on disk. Output `[source file] → [missing target]`.

**B. Orphan files** — every note file under `data/memory/` that no `.md` anywhere in the tree links to. Exclude `index.md` files themselves and anything already under `archived/`.

**C. Stale references** — index entries whose target file signals it is no longer active: clear completion / superseded markers (`✅`, `done`, `completed`, `finito`, `terminé`, `archived`, `superseded by`, `migrated to`), or a last-updated date inside the file far older than the index's own `_Updated:`. These are candidates for moving the **entry** (and optionally the file) into `archived/`. Do not delete the file.

**D. Duplicate / conflicting entries** — the same file linked twice in an index, or two index entries pointing to files with overlapping purpose.

### 3. Structure analysis
Assess the tree shape and flag what is structurally unhealthy:

- **Index bloat** — if root `index.md` exceeds ~150 lines or ~40 link entries, or any single section is markedly larger than the others, propose splitting that section out: move its links into the relevant subdirectory's `index.md` and leave a single pointer line in the root. Apply the same test recursively to subdirectory indexes.
- **Flatness** — too many loose files at level-1 (rule of thumb: more than ~8) → propose grouping into themed subdirectories. Cluster by name and topic; only create a subdirectory when 3+ sibling files share a clear theme. Never create a subdirectory for a single file.
- **Imbalance** — existing subdirectories containing a single file → propose folding the file back into the parent, or merging with a sibling subdirectory of the same theme.
- **Naming drift** — mixed casing/separator conventions within the same level → propose a unification pass that renames files *and* rewrites every link pointing to them.

### 4. Propose a target layout
Present **one** concrete target: the final tree (directories + which files land where), the repaired index outline (entries added, removed, moved to a sub-index), and the full list of dead links to drop. Group every action under one of:

- 🔧 **Repair** (mechanical, safe) — drop dead links, add discovered orphans to the appropriate section, fix path typos, normalise names + their inbound links.
- 📦 **Archive** — move stale-reference entries into `archived/index.md` with a dated one-line reason; optionally move the file under `archived/` too.
- ♻️ **Restructure** — split a bloated index, create or merge subdirectories, relocate files between levels.
- ❓ **Confirm** — anything genuinely ambiguous (an orphan that may be intentionally unlinked; a rename of a well-known path; a file whose staleness is uncertain).

Wait for explicit user approval of the plan before touching anything. ❓ items always require a per-item answer; 🔧/📦/♻️ can be approved wholesale ("yes, do all repairs").

### 5. Execute
Apply the approved plan in this order to keep paths consistent at every step:

1. **Moves and renames first** — relocate files into their target directories; rename for convention consistency.
2. **Rewrite link targets** across every `.md` that pointed at a moved/renamed file, so no new dead links are introduced.
3. **Create/rewrite subdirectory `index.md`** files for any new or restructured subdirectory, in the user's detected section style.
4. **Rewrite root `index.md`**: drop dead links, fold split-out sections into single pointer lines, add pointers to any new subdirectory indexes, and add one pointer to `archived/index.md` if it was created.
5. **Append dated entries** to `data/memory/archived/index.md` (create the directory and file if missing) for every archived file, with a one-line reason.
6. **Bump `_Updated:`** to today's date in every index you touched.

## Rules
- **Never delete** a file without explicit confirmation. Prefer archiving.
- **Never edit note bodies** — you move files and rewrite indexes/links only.
- **Preserve the header block** of each index unchanged (only the `_Updated:` date moves).
- **Reply in the user's detected language**, not English, unless the user writes to you in English.
- **Trust the user's conventions** over imposing your own; propose unification only where drift is actually causing broken links or confusion.
- Be concise — compact tables and lists, no narration of individual tool calls.
