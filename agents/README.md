# Agent icons — style guide

Each agent in the `agents/` directory can have an icon/avatar declared in the `"icon"` field of its `meta.json`. The backend serves the file via `GET /api/agents/{id}/icon`.

## Visual style

Icons were generated with **xAI Grok Imagine** in a **concept art / character design** style:

- **Style**: illustrated, not photorealistic, not flat vector, not anime
- **Technique**: bold brushstrokes, rich colours, depth, video game concept art quality (Overwatch / Arcane / Hades)
- **Format**: portrait (vertical rectangle)
- **Background**: medium-bright, not dark, no neon
- **Subject**: a character / living being representing the agent's role, with contextual elements (tools, holograms, symbols)
- **Palette**: varies per agent, generally warm with one dominant colour

## Base prompt template

```
Stylized character portrait of an AI agent called "{NAME}".
Concept art style with bold brushstrokes and rich colors.
{character description and surrounding visual elements}
{dominant colours}
Illustrated character design, not photorealistic, not flat vector, not anime.
Video game concept art quality.
Portrait format, vertical.
High detail, expressive.
```

## Per-agent reference

| Agent | Subject | Palette |
|-------|---------|---------|
| **Architect** | Visionary with floating architectural blueprints and geometry | Blue & teal |
| **Engineer** | Technician/cyborg with holographic tools, gears, circuits | Amber & steel blue |
| **Explorer** | Curious analyst with magnifying glass, floating code and data trails | Deep blue & gold |
| **Researcher** | Scientist with smart glasses, floating documents, magnifier | Purple & teal |
| **Main Assistant** | Central charismatic leader with luminous geometric shapes | Purple & gold |
| **TIC** | Mysterious figure with multiple eyes, radar, data nodes | Dark purple & cyan |
| **Tinker** | Clever craftsperson with multitool, gears, repair tools | Orange & steel grey |
| **Worker** | Practical person with futuristic toolbelt and mechanical elements | Orange & steel grey |
| **Blueprint** | Scholarly figure with floating scrolls and glowing quills writing words in mid-air, luminous documents orbiting | Deep indigo & burnished gold |
| **Tech Lead** | Confident strategist at a holographic kanban board, task cards floating mid-air, sub-agents visible in the background | Warm amber & deep teal |
| **Project Coordinator** | Central orchestrator with glowing connected nodes, satellite sub-agents orbiting, holographic project maps and branching task flows | Teal & warm gold |


## Adding a new agent icon

1. Generate the image using the prompt template above
2. Save it as `agents/{agent_id}/icon.png`
3. Add `"icon": "icon.png"` to the agent's `meta.json`
4. No code changes needed — the backend serves whatever file path is declared in the manifest
