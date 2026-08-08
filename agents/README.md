# Agents

## Adding a new agent: the skills index is opt-in

An agent sees the installed skills **only** if its `AGENT.md` carries the
`<!-- SKILLS_LIST -->` placeholder, normally through
`<!-- INCLUDE: common/skills.md -->`. There is no `meta.json` flag: the sentinel
*is* the switch, exactly as it is for `<!-- MCP_LIST -->`.

So a new agent starts **without** the index and stays without it until someone
adds the line. That is the deliberate direction of the default: the opposite one
— an agent inheriting the index by forgetfulness — is the worse failure, because
the index is written in the imperative ("you MUST read its SKILL.md") and an
unattended `type: system` agent has its approvals auto-denied and sometimes no
tools at all.

`common/skills.md` is **one line and deliberately holds no prose**, unlike
`common/mcp.md`. Every word — the imperative header, the list, the closing rules
— is produced by the renderer, so that an instance with no skills installed gets
an empty string instead of a header promising a list that isn't there. (That is
not hypothetical: the MCP section keeps its prose in the fragment, and its empty
state once had the model invent a discovery tool to fill the gap.) The fragment
cannot explain itself in place either — `resolve_includes` copies any line that
is not an upper-case sentinel straight into the prompt, so a comment there would
be read by the model.

The rule of thumb: a `chat` or `task` agent gets the include, a `system` agent
does not. Put the line **as low as possible** in the prompt (by convention right
after `common/mcp.md`) — anything above it survives in the provider's cached
prefix when a skill is added or removed. `crates/skald-core/src/agents.rs` has a
test that holds every shipped agent to this.

# Agent icons — style guide

Each agent in the `agents/` directory can have an icon/avatar declared in the `"icon"` field of its `meta.json`. The backend serves the file via `GET /api/agents/{id}/icon`.

## Visual style

Icons are generated with **Vector Paintings** LoRA via ComfyUI in a warm, family-friendly style:

- **Style**: painterly vector — bold shapes fused with expressive brushstrokes
- **Technique**: vivid colours, emotion, motion, warm lighting
- **Format**: square (1024×1024), rendered as a character portrait
- **Background**: warm, cozy, medium-bright (no dark/no neon)
- **Subject**: a warm animal character representing the agent's role, with contextual elements (tools, symbols, objects)
- **Palette**: terracotta, amber, warm gold, coral, soft teal — warm and inviting
- **Trigger word**: `VectorPaintDaal` must be included at the start of the prompt

## Prompt template

```
VectorPaintDaal. A warm friendly {ANIMAL} character with a gentle smile, wearing {CLOTHING/ACCESSORIES}. It holds {OBJECT} and around it float {SYMBOLS}. Warm golden light, cozy atmosphere. {DOMINANT_COLOURS} palette. Expressive bold brushstrokes, painterly vector style. Family-friendly illustration, portrait of a kind {ROLE}.
```

## Per-agent reference

### Chat agents — warm animals

| Agent | Animal | Role | Elements | Palette |
|-------|--------|------|----------|---------|
| **Main Assistant** 🦊 | Fox | General assistant | Glowing threads connecting a heart, star, house | Terracotta, amber, gold |
| **Project Coordinator** 🦡 | Badger | Family coordinator | Floating threads linking heart, star, house, smiling face; cozy kitchen table | Terracotta, amber, gold, coral |
| **Researcher** 🐿️ | Squirrel | Curious researcher | Glowing book, magnifying glass, compass, scrolls, stars | Terracotta, amber, soft teal, coral |
| **Generalist** 🦫 | Beaver | Handy executor | Glowing multitool, wrench, paintbrush, trowel, cooking pot | Terracotta, orange, amber, timber |
| **Code Explorer** 🕵️ | Meerkat | Curious analyst | Magnifying glass, data trails, sparkling code symbols | Terracotta, amber, deep blue, gold |
| **Software Architect** 🏗️ | Heron | Thoughtful planner | Floating blueprints, geometric shapes, building blocks | Terracotta, soft teal, amber, pale gold |
| **Software Engineer** 🔧 | Bear | Focused builder | Glowing wrench, gears, circuit board, hammer, sparks | Terracotta, orange, amber, steel grey |
| **Spec Writer** 📝 | Owl | Wise scribe | Glowing quill, scrolls, open books, words floating mid-air | Deep indigo, burnished gold, amber, cream |
| **Tech Lead** 👑 | Stag | Confident strategist | Holographic kanban board, task cards, sub-agent symbols | Warm amber, deep teal, gold, coral |
| **Business Analyst** 💼 | Magpie | Thoughtful evaluator | Glowing clipboard, floating documents, abacus, data points | Deep indigo, gold, soft teal, amber |
| **Companion** 🦦 | Otter | Children's friend | Glowing pencil, smiling sun, star, open book, paintbrush | Soft coral, amber, gold, gentle teal |

### System agents — insect family

System agents (`type: "system"`) are invisible background agents that maintain the platform. They use insect characters to visually distinguish them from chat-facing agents.

| Agent | Animal | Role | Elements | Palette |
|-------|--------|------|----------|---------|
| **Event triage** 👁️ | Spider 🕷️ | Watchful guardian | Sensor nodes, glowing web, radar arcs, notification symbols (bell, letter, calendar) | Dark purple, amber, soft cyan, warm grey |
| **Private Memory Lint** 🧹 | Firefly ✨ | Private memory caretaker | Glowing lantern, memory fragments, tiny notes, sparkles | Warm gold, amber, soft teal, gentle green |
| **Shared Memory Lint** 🧹 | Bee 🐝 | Shared space caretaker | Scroll with guidelines, honey dipper, honeycomb shapes, tiny documents | Warm amber, gold, soft teal, honey |

## Adding a new agent icon

1. Generate the image using the Vector Paintings prompt template above (include `VectorPaintDaal` at the start)
2. Save it as `agents/{agent_id}/icon.png`
3. Add `"icon": "icon.png"` to the agent's `meta.json` (if not already present)
4. No code changes needed — the backend serves whatever file path is declared in the manifest
