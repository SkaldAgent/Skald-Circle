# /ideagen — structured money-making idea research

Research and validate a business idea. The topic/niche is: {{args}}

The goal is to take a vague topic and produce, in a single working directory, a structured evaluation: candidate ideas → market validation → business plan → stress-test → final recommendation. You are the coordinator: you do **not** write the research artifacts yourself — you dispatch sub-agents and assemble their output. The only file you write yourself is the final synthesis (Phase 7).

## Conventions

- Create **one** working directory `data/ideagen-YYYYMMDD-HHMM/` (use the current date/time) and use it for every artifact in this run.
- Pass this directory as the output location to **every** sub-agent you dispatch.
- Track state with `update_scratchpad` after each phase (at minimum: `phase`, `temp_dir`, `topic`, and the path returned by each sub-agent).
- Track progress with `write_todos`: one task `in_progress` at a time, re-send the whole list on each update.
- All sub-agent dispatches are `mode=sync` — wait for each to finish before deciding the next step.
- Work only from the summaries sub-agents return in their tool-result string. **Never read files from `{temp_dir}/` directly** — that is the executor's job, not yours.

## Phase 1 — Setup

1. Create the working directory `data/ideagen-YYYYMMDD-HHMM/`.
2. `update_scratchpad` with `{ phase: "discovery", temp_dir, topic: "{{args}}" }`.
3. `write_todos` with the phases ahead (one item per phase).

## Phase 2 — Discovery + light validation

Dispatch a **sync** task to `researcher`. Prompt:

> Research money-making ideas in this niche: **{{args}}**.
>
> Find 4-6 concrete ideas. For each, give:
> - What is being sold and to whom
> - Evidence of demand (who pays, roughly how much)
> - Existing solutions and their gaps
> - Estimated barrier to entry (low / medium / high)
> - Rough time-to-first-revenue (weeks / months)
>
> Output a **comparative table** at the top, then one short section per idea.
>
> Write the report to `{temp_dir}/01-ideas.md`.

Save the returned path + summary to scratchpad as `phase2_ideas`.

## Phase 3 — Checkpoint: pick idea(s)

Ask the user via `ask_user_clarification`:

> **Title**: Which idea for **{{args}}**?
>
> **Question**: I found these candidates (details in `{temp_dir}/01-ideas.md`):
> (one line per idea — name + one-sentence why + barrier / time)
>
> Which one(s) should I validate in depth?

**Suggested answers**:
- One option per idea ("Validate idea #N")
- "Validate ideas #N and #M" (multi)
- "Research a different topic: <new topic>"

On "different topic" → restart from Phase 1 with the new topic. Otherwise save the chosen idea(s) to scratchpad as `chosen`.

## Phase 4 — Market validation (deep)

Dispatch a **sync** task to `researcher`. Prompt:

> Do deep market validation for this idea: **<chosen idea from Phase 3>**.
>
> Find:
> - Direct and indirect competitors (with pricing)
> - Market size: TAM / SAM / SOM with the reasoning, not just numbers
> - Trends: growing, stagnating, shrinking? By how much?
> - Competitive moats that exist (or do not)
> - Regulatory / legal constraints
>
> Write the report to `{temp_dir}/02-market.md`.

Save the returned path + summary to scratchpad as `phase4_market`.

## Phase 5 — Business plan (informed)

Dispatch a **sync** task to `generalist`. Prompt:

> Write a business plan draft for this idea: **<chosen idea>**.
>
> The plan MUST be informed by this market validation:
> <paste the Phase 4 summary>
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

Save the returned path + summary to scratchpad as `phase5_plan`.

## Phase 6 — Stress test (critique)

Dispatch a **sync** task to `business-analyst`. Prompt:

> Stress-test this business idea and plan against the market evidence.
>
> Idea: **<chosen idea>**
>
> Business plan summary:
> <paste the Phase 5 summary>
>
> Market validation summary:
> <paste the Phase 4 summary>
>
> Run your full critique framework. Write the report to `{temp_dir}/04-critique.md`.

Save the returned path + verdict + confidence to scratchpad as `phase6_critique`.

## Phase 7 — Final synthesis (you write it)

Assemble `{temp_dir}/05-final.md` **yourself** from the summaries in your scratchpad. Do **not** dispatch a sub-agent for this — you already have everything you need. Structure:

```markdown
# Final report: <chosen idea>

_Date: YYYY-MM-DD_

## Recommendation: GO / NEEDS-MORE-RESEARCH / PIVOT / NO-GO

**Confidence: X/10** — <one line>

## The refined idea

<2-4 sentences incorporating the fixes the critique proposed>

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

- `01-ideas.md` — candidate ideas
- `02-market.md` — market validation
- `03-plan.md` — business plan
- `04-critique.md` — stress test
```

## Phase 8 — Checkpoint: what next

Ask the user via `ask_user_clarification`:

> **Title**: Results for **{{args}}**
>
> **Question**: Recommendation: **<verdict>** (confidence X/10). Key risks: <top 3, one line each>.
>
> Full reports in `{temp_dir}/`. What now?

**Suggested answers**:
- "Looks good, I'll take it from here" → done.
- "Iterate — refine the plan with: <changes>" → back to Phase 5 with the changes, then 6 → 7 → 8 again.
- "Iterate — re-check the market with: <new angle>" → back to Phase 4, then 5 → 6 → 7 → 8 again.
- "Scrap and research a different topic: <new topic>" → restart Phase 1.
- "Save to memory" → append a one-paragraph summary to `data/memory/ideagen-ideas.md`, then done.

If the user wants to iterate again after one loop, recommend stopping: further iteration has diminishing returns — present the best version so far and let them decide.

## Rules

- **Never read files from `{temp_dir}/`** — work from the summaries sub-agents return.
- **Always pass `{temp_dir}` as the output location** to every sub-agent you dispatch.
- Use `ask_user_clarification` at every checkpoint; never make silent assumptions.
- If a sub-agent fails or returns empty results, ask the user whether to retry or pivot.
- Stay in the user's language.
