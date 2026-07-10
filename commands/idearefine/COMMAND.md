# /idearefine — sharpen and validate a single rough idea

Take ONE fuzzy seed idea and turn it into a refined, stress-tested concept. The seed is: {{args}}

The goal is the opposite of `/ideagen` (which funnels a NICHE → many ideas → one): here the user already has ONE concept in mind but it is vague, mixed with questions and constraints. You reframe it, explore distinct angles on the SAME concept, pick the strongest, validate, plan, and stress-test. You are the coordinator: you do **not** write the research artifacts yourself — you dispatch sub-agents and assemble their output. The only files you write yourself are the reframe (Phase 2) and the final synthesis (Phase 8).

## Conventions

- Create **one** working directory `data/idearefine-YYYYMMDD-HHMM/` (use the current date/time) and use it for every artifact in this run.
- Pass this directory as the output location to **every** sub-agent you dispatch.
- Track state with `update_scratchpad` after each phase (at minimum: `phase`, `temp_dir`, `seed`, `reframe_summary`, and the path returned by each sub-agent).
- Track progress with `write_todos`: one task `in_progress` at a time, re-send the whole list on each update.
- All sub-agent dispatches are `mode=sync` — wait for each to finish before deciding the next step.
- Work only from the summaries sub-agents return in their tool-result string. **Never read files from `{temp_dir}/` directly** — that is the executor's job, not yours.

## Phase 1 — Setup

1. Create the working directory `data/idearefine-YYYYMMDD-HHMM/`.
2. `update_scratchpad` with `{ phase: "reframe", temp_dir, seed: "{{args}}" }`.
3. `write_todos` with the phases ahead (one item per phase).

## Phase 2 — Reframe the seed (you do this)

Read `{{args}}` and extract a structured reframe. Write it to `{temp_dir}/00-reframe.md` **yourself** (no sub-agent). Structure:

```markdown
# Reframe: {{args}}

## Core concept
<1-2 crisp sentences: what is actually being proposed>

## Anchors (non-negotiable)
- <elements that MUST stay in the final idea, e.g. "MCP-based", "agent-to-agent", "marketplace model">

## Analogues to beat
- <named incumbents the user wants to differentiate from, e.g. eBay, craigslist>

## Open questions (must be answered by the final report)
1. <every "?" in the seed, plus implicit "what makes it unique" asks>

## Assumptions to test
- <unstated premises, e.g. "agents have budgets", "demand exists for X">
```

Then checkpoint via `ask_user_clarification`:

> **Title**: Reframe check for "{{args}}"
>
> **Question**: I read your seed as:
> - Core concept: <…>
> - Anchors: <…>
> - Analogues to beat: <…>
> - Open questions: <list>
>
> Is this accurate? Anything to add, correct, or drop?

**Suggested answers**:
- "Correct, proceed"
- "Adjust: <change>" → rewrite `00-reframe.md` and re-checkpoint (loop until confirmed).

Save the confirmed reframe summary to scratchpad as `reframe_summary`.

## Phase 3 — Explore angles on the concept

Dispatch a **sync** task to `researcher`. Prompt:

> The user has ONE concept in mind and wants distinct angles to sharpen it — NOT disparate ideas. The confirmed reframe:
>
> <paste reframe_summary: core concept, anchors, analogues, open questions>
>
> Find 4-6 distinct ANGLES or VARIATIONS of this same concept. Each angle is a different hypothesis about what could make it uniquely valuable. For each:
> - The angle (1 sentence — a specific positioning/feature/wedge bet)
> - Who it specifically serves (more precise than the seed)
> - Wedge vs EACH named analogue: why a user picks this over <analogue>
> - Riskiest assumption behind this angle
> - Cheapest way to test it in <2 weeks
>
> Output a **comparative table** at the top, then one section per angle. Address the open questions where relevant.
>
> Write the report to `{temp_dir}/01-angles.md`.

Save the returned path + summary to scratchpad as `phase3_angles`.

## Phase 4 — Checkpoint: pick angle(s)

Ask the user via `ask_user_clarification`:

> **Title**: Which angle for "{{args}}"?
>
> **Question**: I explored these angles (details in `{temp_dir}/01-angles.md`):
> (one line per angle — name + wedge + riskiest assumption)
>
> Which angle(s) should I develop in depth?

**Suggested answers**:
- One option per angle ("Develop angle #N")
- "Develop angles #N and #M" (multi — max 2)
- "Explore more with lens: <lens>" → back to Phase 3 with the new lens
- "Pivot the concept: <change>" → back to Phase 2 with the change

Save the chosen angle(s) to scratchpad as `chosen`.

## Phase 5 — Market validation + positioning

Dispatch a **sync** task to `researcher`. Prompt:

> Do deep market validation for this angle: **<chosen angle from Phase 4>**.
>
> Reframe context: <paste reframe_summary>
>
> Find:
> - Direct and indirect competitors (with pricing)
> - Market size: TAM / SAM / SOM with the reasoning, not just numbers
> - Trends: growing, stagnating, shrinking? By how much?
> - Regulatory / legal constraints
> - **Differentiation matrix** — rows: this idea + each named analogue (<analogues from reframe>); columns: what's sold, who, trust model, pricing, distribution, defensible moat
> - **Wedge defensibility** — what can each incumbent NOT copy in <12 months, and why
>
> Write the report to `{temp_dir}/02-market.md`.

Save the returned path + summary to scratchpad as `phase5_market`.

## Phase 6 — Business plan (informed)

Dispatch a **sync** task to `generalist`. Prompt:

> Write a business plan draft for: **<chosen angle>**.
>
> It MUST be informed by this market validation:
> <paste the Phase 5 summary>
>
> And respect these anchors: <paste anchors from reframe>
>
> Cover:
> 1. Value proposition — what problem, for whom
> 2. Product / service — what exactly, how delivered
> 3. Revenue model & pricing — grounded in competitor pricing above
> 4. Target customer — who pays, B2B / B2C / niche
> 5. Go-to-market — how you reach them, realistically
> 6. Timeline — MVP, first paying customer, break-even
> 7. Resources — skills, tools, upfront cost
>
> Write the result to `{temp_dir}/03-plan.md`.

Save the returned path + summary to scratchpad as `phase6_plan`.

## Phase 7 — Stress test (critique)

Dispatch a **sync** task to `business-analyst`. Prompt:

> Stress-test this idea and plan against the market evidence.
>
> Angle: **<chosen angle>**
>
> Reframe (incl. open questions and analogues): <paste reframe_summary>
>
> Business plan summary:
> <paste the Phase 6 summary>
>
> Market validation summary:
> <paste the Phase 5 summary>
>
> Run your full critique framework. Explicitly check whether each OPEN QUESTION from the reframe has been answered, and whether the wedge survives the critique. Write the report to `{temp_dir}/04-critique.md`.

Save the returned path + verdict + confidence to scratchpad as `phase7_critique`.

## Phase 8 — Final synthesis (you write it)

Assemble `{temp_dir}/05-final.md` **yourself** from the summaries in your scratchpad. Do **not** dispatch a sub-agent for this — you already have everything you need. Structure:

```markdown
# Final report: <chosen angle>

_Date: YYYY-MM-DD_

## Recommendation: GO / NEEDS-MORE-RESEARCH / PIVOT / NO-GO

**Confidence: X/10** — <one line>

## The refined idea

<2-4 sentences: the original seed, now sharpened with the chosen angle and the fixes the critique proposed>

## Wedge vs <analogues>

<why a user picks this over each named analogue — 1 line each>

## Open questions → answered

| Question (from reframe) | Answer | Where |
| --- | --- | --- |
| <q1> | <a1> | <section/artifact> |

## Key risks (top 3)

1. …
2. …
3. …

## Break-even estimate

- Time to first paying customer: …
- Time to break-even: …
- Upfront capital: …

## Next 3 concrete steps (if GO)

1. …
2. …
3. …

## Artifacts

- `00-reframe.md` — confirmed reframe
- `01-angles.md` — explored angles
- `02-market.md` — market validation + positioning
- `03-plan.md` — business plan
- `04-critique.md` — stress test
```

## Phase 9 — Checkpoint: what next

Ask the user via `ask_user_clarification`:

> **Title**: Results for "{{args}}"
>
> **Question**: Recommendation: **<verdict>** (confidence X/10). Wedge: <one line>. Key risks: <top 3, one line each>.
>
> Full reports in `{temp_dir}/`. What now?

**Suggested answers**:
- "Looks good, I'll take it from here" → done.
- "Iterate — refine the plan with: <changes>" → back to Phase 6, then 7 → 8 → 9.
- "Iterate — re-check the market with: <new angle>" → back to Phase 5, then 6 → 7 → 8 → 9.
- "Re-explore angles with: <lens>" → back to Phase 3.
- "Pivot the concept: <change>" → back to Phase 2.
- "Save to memory" → append a one-paragraph summary to `data/memory/idearefine-ideas.md`, then done.

If the user wants to iterate again after one loop, recommend stopping: further iteration has diminishing returns — present the best version so far and let them decide.

## Rules

- **Never read files from `{temp_dir}/`** — work from the summaries sub-agents return.
- **Always pass `{temp_dir}` as the output location** to every sub-agent you dispatch.
- Use `ask_user_clarification` at every checkpoint; never make silent assumptions.
- The reframe (Phase 2) and the final synthesis (Phase 8) are written by **you**, not a sub-agent.
- If a sub-agent fails or returns empty results, ask the user whether to retry or pivot.
- Stay in the user's language.
