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
| **TIC** 👁️ | Spider 🕷️ | Watchful guardian | Sensor nodes, glowing web, radar arcs, notification symbols (bell, letter, calendar) | Dark purple, amber, soft cyan, warm grey |
| **Private Memory Lint** 🧹 | Firefly ✨ | Private memory caretaker | Glowing lantern, memory fragments, tiny notes, sparkles | Warm gold, amber, soft teal, gentle green |
| **Shared Memory Lint** 🧹 | Bee 🐝 | Shared space caretaker | Scroll with guidelines, honey dipper, honeycomb shapes, tiny documents | Warm amber, gold, soft teal, honey |

## Adding a new agent icon

1. Generate the image using the Vector Paintings prompt template above (include `VectorPaintDaal` at the start)
2. Save it as `agents/{agent_id}/icon.png`
3. Add `"icon": "icon.png"` to the agent's `meta.json` (if not already present)
4. No code changes needed — the backend serves whatever file path is declared in the manifest
