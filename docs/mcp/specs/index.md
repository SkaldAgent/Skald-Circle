# MCP Specification Reference

Quick-reference mirrors of each **official Model Context Protocol specification revision**,
maintained as background for future Skald MCP work. The authoritative source of truth for
every revision is the TypeScript schema (`schema/<version>/schema.ts`) in
[`modelcontextprotocol/specification`](https://github.com/modelcontextprotocol/specification);
these notes summarize each revision's structure, requirements, and deltas for fast lookup
when implementing against `crates/mcp-client/` and `src/core/mcp/`.

Each file follows the same template — *At a glance → Architecture → Base Protocol
(Messages / Lifecycle / Transports / Authorization / Versioning) → Server Features →
Client Features → Changes vs previous → Skald relevance → References* — so revisions can be
diffed section by section.

## Revisions

| Revision | Status | Released | Protocol style | Headline | File |
| --- | --- | --- | --- | --- | --- |
| **2026-07-28** | **Draft / RC** | ~2026-05 (pre-release) | Stateless, per-request caps | Foundational redesign: stateless base, per-request negotiation, extensions framework; Sampling/Roots/Logging deprecated | [2026-07-28-draft.md](2026-07-28-draft.md) |
| **2025-11-25** | **Latest Stable** | 2025-11-25 | Stateful | OIDC Discovery, icons, URL-mode elicitation, sampling tool-calling, experimental Tasks | [2025-11-25.md](2025-11-25.md) |
| **2025-06-18** | Stable | 2025-06-18 | Stateful | Streamable HTTP, Elicitation, structured tool output, OAuth Resource Indicators (RFC 8707) | [2025-06-18.md](2025-06-18.md) |
| **2024-11-05** | Legacy | 2024-11-05 | Stateful | First public release: stdio + HTTP+SSE, Resources/Prompts/Tools/Logging, Sampling/Roots | [2024-11-05.md](2024-11-05.md) |

Additional repo tags not given their own file (point-revisions of the spec lines above):

- `2024-11-05-final` — final revision of the 2024-11-05 line (covered in [2024-11-05.md](2024-11-05.md)).
- `2024-10-07` — pre-public preliminary tag, superseded by the public 2024-11-05 release.
- `2025-03-26` — intermediate revision (deprecated the HTTP+SSE transport); superseded by 2025-06-18.
- `2025-11-25-RC` — release candidate of the 2025-11-25 line.

## Feature availability across revisions

| Capability | 2024-11-05 | 2025-06-18 | 2025-11-25 | 2026-07-28 (draft) |
| --- | :---: | :---: | :---: | :---: |
| **stdio transport** | ✔ | ✔ | ✔ | ✔ |
| **HTTP+SSE transport** | ✔ | ✘ (replaced) | ✘ | ✘ (deprecated since 2025-03-26) |
| **Streamable HTTP transport** | ✘ | ✔ | ✔ | ✔ |
| **Stateful connections** | ✔ | ✔ | ✔ | ✘ (stateless, self-contained requests) |
| **Per-request capability negotiation** | ✘ | ✘ | ✘ | ✔ |
| **Resources** | ✔ | ✔ | ✔ | ✔ |
| **Prompts** | ✔ | ✔ | ✔ | ✔ |
| **Tools** (structured output `outputSchema`/`structuredContent`) | ✘ | ✔ | ✔ | ✔ |
| **Tools** (unstructured `content[]` only) | ✔ | ✔ | ✔ | ✔ |
| **Logging** (server feature / utility) | ✔ | ✔ | ✔ | ✘ deprecated (→ stderr / OpenTelemetry) |
| **Icons metadata** | ✘ | ✘ | ✔ | ✔ |
| **Sampling** | ✔ | ✔ | ✔ | ✘ deprecated (SEP-2577) |
| **Roots** | ✔ | ✔ | ✔ | ✘ deprecated (SEP-2577) |
| **Elicitation** (form mode) | ✘ | ✔ | ✔ | ✔ (only core client feature) |
| **Elicitation** (URL mode) | ✘ | ✘ | ✔ | ✔ |
| **OAuth 2.1 authorization framework** | ✘ (not in core spec) | ✔ (RFC 8707, RFC 9728) | ✔ + OIDC Discovery + Client ID metadata | ✔ (DCR deprecated → Client ID metadata) |
| **JSON-RPC batching** | ✔ | ✘ (removed) | ✘ | ✘ |
| **Tasks** | ✘ | ✘ | experimental | extension `io.modelcontextprotocol/tasks` |
| **Extensions framework** | ✘ | ✘ | ✘ | ✔ |

## Feature-lifecycle policy (introduced in the 2026-07-28 draft)

The draft formalizes how features are deprecated and removed ([SEP-2577](https://modelcontextprotocol.io/specification/draft/deprecated)):

- A deprecated feature is retained for **at least 12 months** and remains usable for interop.
- It becomes **eligible for removal** in the first revision released on/after **2027-07-28**.
- Currently deprecated: **Sampling**, **Roots**, **Logging**, **Dynamic Client Registration**.

When implementing against the draft, treat these as present-but-don't-build-new-dependencies.

## Source of truth

- **Schema (authoritative):** [`schema/<version>/schema.ts`](https://github.com/modelcontextprotocol/specification/tree/main/schema) — the TypeScript schema is normative; the prose restates it with BCP 14 keywords (MUST / SHOULD / MAY).
- **Spec site:** [modelcontextprotocol.io/specification](https://modelcontextprotocol.io/specification) — per-version browsable spec.
- **Releases:** [github.com/modelcontextprotocol/modelcontextprotocol/releases](https://github.com/modelcontextprotocol/modelcontextprotocol/releases) — tags, RCs, changelogs.

## Related Skald docs

- [../index.md](../index.md) — MCP subsystem overview (servers, transports, integration)
- [../../mcp.md](../../mcp.md) — `McpManager`, transports, naming convention, enable/disable
