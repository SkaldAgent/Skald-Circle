# System agents

A **system agent** is an assistant that runs in the background on someone's behalf, without being asked. Nobody starts it and nobody is waiting for its answer: it wakes up on a schedule, looks at something, and gets in touch only if there is a reason to.

There are three:

| Agent | What it watches | How often |
| --- | --- | --- |
| **Event triage** | events arriving from that person's connectors | every few minutes |
| **Private memory lint** | that person's own memory notes | weekly |
| **Shared memory lint** | the group's shared memory | weekly |

They share three habits worth stating once, because they explain most of what people ask:

- **They only read and report.** None of them changes anything. If something needs doing, they say so and the person decides.
- **An empty run is a correct run.** They are not supposed to find something every time, and they stay quiet when they don't.
- **They run per person, on that person's own things**, with one exception noted below.

## Event triage

Connectors (Gmail, a calendar, WhatsApp…) push events into the system as they happen — a new message arrives, a meeting is moved. Those events pile up quietly; nothing interrupts anyone.

Every so often event triage wakes up and reads the batch that accumulated since last time. For each event it decides whether it is worth the interruption, using what it knows about that person from their private memory: who matters to them, what they are working on, what they have said they want to be told about. Events that pass become notifications in their Inbox. Events that don't are simply marked as seen — a newsletter or a group chat with nothing relevant in it produces nothing.

The name is the limit of the job: it **sorts**, it never acts. It will not reply to a message or move a calendar event. If an event needs an action, it says so in the notification and the person decides.

## The two memory lints

Memory is kept as a small wiki rather than a pile of notes (see [memory.md](memory.md)): notes cross-reference each other, an `index.md` says where things are, and a `log.md` records every change. That works while somebody maintains it — and quietly rots when nobody does. Contradictions stay unresolved, dates go by, notes lose the last line that pointed at them, the same fact ends up written twice in two places that slowly disagree.

The lints are the scheduled maintenance pass. Once a week they re-read a store and report what has drifted:

- facts whose date has passed — a renewal now due, a plan that already happened
- questions somebody was asked to confirm and never did
- notes nothing links to any more, and index lines pointing at notes that no longer exist
- two notes saying the same thing, especially when they have started to disagree

**They never fix anything.** They report, and a person decides. This is deliberate: an automated pass reading a store several people built over months is guessing, and a wrong guess destroys something somebody meant. Reading a report costs thirty seconds; a wrong edit can lose a fact nobody notices is gone until they need it.

There are two of them because the two stores are not the same job.

**Private memory lint** runs for each person over their own notes, and reports to them alone.

**Shared memory lint** runs once over the group's shared store, and looks for one extra thing that only exists there: **a note that fails the table rule** — one person's private business written somewhere every member can read. The rule is that something belongs in shared memory only if you would say it out loud with every member in the room; health, school results, money, worries and one member's opinion of another do not. When it finds one it says *which note* and *what kind of problem*, without repeating the sensitive content — restating it in a notification would spread it further, which is exactly the harm being flagged.

The shared store belongs to nobody in particular, so that pass runs **as the admin** and its report goes to them. That is about who can act on it, not about privacy: everything in shared memory is already readable by every member.

## Why a run can be missing

Users are handled one at a time, and a user is **skipped** if they have not logged in since the server last restarted.

This is not a fault, it is how the encryption works: a person's data is unreadable until they log in and their password unlocks it. Until that happens there is nothing to read and nowhere to write. Nothing is lost — events keep accumulating, and the first run after they log in picks up everything waiting.

So if someone asks "why didn't it tell me about that email from this morning?", the first thing to check is whether they had logged in at the time. The same applies to the shared memory lint: it needs an admin who has logged in since the restart.

Schedules are counted **per person from their own last run**, and they survive a restart — so a weekly pass stays weekly even on a machine that gets rebooted every few days.

## The System agents page

Sidebar → **System agents**. There is one tab per agent, plus **All**. A tab holds that agent's description, its settings (admin only), and its run history — because "why did this do nothing last night?" is usually half a settings question and half a log question.

Each row is one run, newest first:

- **Started** and **Duration**.
- **Status** — completed, failed, or still running.
- **Result** — the agent's own counters (events looked at, notes read, notifications sent), or the error if it failed.

Clicking a row opens the conversation the run happened in, for anyone who wants to see the reasoning.

A run appears **only when there was something to look at**. Long gaps mean quiet connectors or an untouched memory store, not a broken agent.

**The run history is personal.** Each run is written into that user's own encrypted database, so every user — the admin included — sees their own runs and nobody else's. There is no instance-wide view.

## What the admin can change

Each agent's tab carries the same three settings, visible only to an admin:

- **Enabled** — turns that agent on or off for the whole instance, for everyone.
- **Interval** — how long between passes for each person. Event triage is in minutes, the lints in days.
- **Security group** — which tools the agent may use during a run. It is re-checked against each user's own role: if their role does not allow that group, their run uses their role's default group instead. Nobody's background agent gets more access than their role would give them.

There is no per-user on/off switch: if an agent is enabled, it runs for everyone who has logged in.
