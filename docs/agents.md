# Agents

An **agent** is a role the assistant can play: a name, an icon, a system prompt that shapes its personality and skills, and a set of tools. Every conversation with the assistant is a conversation with **one** agent — the same engine, a different persona.

Agents are defined as plain files in the `agents/` folder on the server (one subfolder per agent: a `meta.json` with the name and description, an `AGENT.md` with the prompt, optionally an icon). The app discovers them at startup and **re-reads the prompt files on every use**, so editing an agent's `AGENT.md` on the server takes effect without restarting anything.

There are three kinds of agent, and the difference is *who starts the conversation*:

| Kind | Who talks to it | Count |
|------|-----------------|-------|
| **Chat** | You, directly | 3 |
| **Task** | The assistant, on your behalf (delegation) | 8 |
| **System** | Nobody — it runs on a schedule, in the background | 4 |

## Which agent are you talking to?

When you start a conversation, which chat agent it lands on is decided by your **role** — not by you picking one from a list:

- Members of most roles get the **Assistant** — the general-purpose agent that helps with anything, remembers what matters in memory, and delegates specialised work (see below).
- Members of the **children's role** get the **Companion** — a warmer, gentler assistant that adapts its tone and vocabulary to the child's age.
- Conversations about a **project** get the **Project Coordinator** — the same agent who runs the project's chat, holding the project's full context.

An admin can change a role's default assistant in the role editor (sidebar → **Roles** → edit a role → **Default assistant**). Leaving it empty means "the Assistant". A role change applies to **new** conversations, from the member's next login — an existing conversation keeps the agent it started with.

If a user asks "who am I talking to?" or "why does my assistant talk differently from theirs?", the answer is: the agent their role defaults to, and it can be changed by the admin — not by the user, and not by asking the assistant (the assistant cannot change its own role).

## The Agents page

Sidebar → **Agents** shows every agent on this instance, in three sections (Chat / Task / System), as cards with the agent's icon, name, and a short description.

Clicking an agent opens its detail page with:

- **The prompt** — the full `AGENT.md` text the agent runs under, rendered as Markdown. This is not secret: it is what the agent is told to be and do, and reading it is a good way to understand why an agent behaves the way it does. (The Assistant's prompt, for example, tells it to keep personal facts in memory, to prefer `user-memory/` notes, and to delegate to task agents when a job needs a specialist.)
- **The models** — which LLMs the agent can run on, and how it picks one (see [How the model is chosen](#how-the-model-is-chosen)).

The System section lists the background agents too — they are invisible in the chat but visible here, with their prompts. For what they *do* and when they run, see [system-agents.md](system-agents.md).

## The task agents: the specialists

Eight agents exist to do specific jobs, and the chat agent calls on them automatically when the job matches — you never talk to them directly. You *trigger* one simply by asking: *"use the researcher to find me…"*, *"get the code explorer to look at…"*, or just describing the task in a way that matches a specialist's job. The assistant recognises the match, delegates, and brings the result back into the conversation.

| Agent | What it is for | Where its output goes |
|-------|----------------|-----------------------|
| **Researcher** | Multi-step web research, with sources | A structured summary in the chat; findings saved to the scratchpad, optionally to `data/research/` |
| **Business Analyst** | Stress-tests a business idea or plan against the evidence you provide; GO / NO-GO / PIVOT verdict | A critique report (path you choose, or the scratchpad) |
| **Code Explorer** | Studies code, investigates bugs, analyses architecture — analysis only, never edits | A structured Markdown report in `data/explorer/` |
| **Spec Writer** | Turns a rough idea into a detailed, unambiguous written specification | A Markdown spec document (never code) |
| **Software Architect** | Plans a code change end-to-end before anything is touched | An implementation plan, possibly delegating the edits to the engineer |
| **Software Engineer** | Writes and edits source files to implement a decided change | The code changes themselves |
| **Tech Lead** | Takes project requirements and builds the whole thing, decomposing the work and orchestrating architect + engineer | The completed implementation |
| **Generalist** | Carries out well-defined hands-on work — file edits, shell commands, batch operations — exactly as instructed | The finished work |

Three things worth knowing about delegation:

- **It is ordinary conversation.** The child agent's work happens in its own context, but what you see is the flow: the assistant calls the specialist, the specialist reports back, the assistant answers you. You can watch it happen in the chat.
- **The specialists have the same rules.** They run with the same tool set, the same security groups and the same approval cards — a specialist wanting to write a file in a shared folder will ask for confirmation exactly as the assistant would. (The Conversation Review agent is the exception by design: it has no tools at all, see [system-agents.md](system-agents.md).)
- **They cannot be summoned from the void.** The assistant decides whether and when to delegate — there is no user-facing list to pick a specialist from, and calling one yourself is not a thing you can do (nor should need to).

## How the model is chosen

Every agent declares a **strength** — how powerful a model it should run on (from *very low* to *very high*). When no model is pinned, the app picks the best available model at or above that strength — so a lightweight background task uses a small model, and the most demanding specialists (Architect, Tech Lead) ask for the strongest.

A specific conversation can **pin** a model instead, per conversation, with the model picker in the chat. Pinning overrides the strength choice for that conversation only.

## Custom agents (admin)

Agents are data, not code — an admin can add a new one by creating a folder on the server:

- `agents/<id>/meta.json` — the name, description, type (`chat`, `task` or `system`), strength, and optionally an icon file.
- `agents/<id>/AGENT.md` — the system prompt (the `name` given in `meta.json` is what users will see; the folder name is the internal id).

No restart and no rebuild: the app discovers the new agent and picks up prompt edits on the next use. An icon (a square image) is served automatically when declared in `meta.json` — optional, but it is what shows on the Agents page and in the chat.

If a user asks for "a different assistant", the honest answer is: there is no UI to create one — an admin can add a custom agent by hand on the server, and this guide tells them how, or the user can be pointed at the role's default-assistant setting instead.

## Notes

- **What the Assistant knows about you.** At the start of a conversation, the chat agent reads a few memory notes automatically: your profile and the private-memory index (`user-memory/`), and the group's shared-memory index. That is *in addition to* whatever you say — it is how the agent "remembers" between conversations. The Project Coordinator additionally reads the project's own `SKALD.md` when one exists, so it arrives already knowing the project. See [memory.md](memory.md).
- **The system agents are invisible on purpose.** They run on a schedule, not in a conversation, and they never talk to you — they notify you when something needs attention, and their work and settings live on the System agents page (`#system-agents`). See [system-agents.md](system-agents.md).
- **Prompts are not secrets.** Nothing on the Agents page is hidden from the user who can see it — if someone asks "what is the assistant told to do?", the answer is "read it on the Agents page".
- **Agents cannot change themselves.** An agent's prompt is a file on the server; the agent cannot edit it, and a user cannot make an agent "become" another agent by asking. New conversations, new agent — the mapping is decided by the role.
