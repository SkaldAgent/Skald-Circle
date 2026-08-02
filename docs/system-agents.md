# System agents

A **system agent** is an assistant that runs in the background on someone's behalf, without being asked. Nobody starts it and nobody is waiting for its answer: it wakes up on a schedule, looks at something, and gets in touch only if there is a reason to.

There are four:

| Agent | What it watches | How often |
| --- | --- | --- |
| **Event triage** | events arriving from that person's connectors | every few minutes |
| **Private memory lint** | that person's own memory notes | weekly |
| **Shared memory lint** | the group's shared memory | weekly |
| **Conversation review** | the conversations of someone who is supervised | nightly |

They share three habits worth stating once, because they explain most of what people ask:

- **They only read and report.** None of them changes anything. If something needs doing, they say so and a person decides.
- **An empty run is a correct run.** They are not supposed to find something every time, and they stay quiet when they don't.
- **They run per person, on that person's own things** — except the last two, which are about the group and about someone else respectively.

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

The shared store belongs to nobody in particular, so the *scheduled* pass runs **as the admin** and its report goes to them. That is about who can act on it, not about privacy: everything in shared memory is already readable by every member — which is also why any member can press **Run now** on it and get the report themselves.

## Conversation review

Some accounts are **supervised**: somebody else has agreed to keep an eye on how that person is getting on with the assistant. A child's account is the usual case, but nothing in the system says "child" — it is a link between two people, and an admin decides who is on either end of it.

Once a night, for each supervised person, this agent reads everything that person and the assistant said to each other since the previous review, and writes **one report** for the people who supervise them.

A few things about it are worth knowing, because they are the questions people actually ask:

- **One report per person, not per conversation.** Somebody may open five chats in a day. The review takes the whole stretch at once, so a subject that came up twice in two different places is something it can notice — reviewing each conversation separately would lose exactly that.
- **It reads what was *said*, not what was *done*.** Messages only. If the assistant ran a search, opened a file or used a connector, none of that is visible to the review — not the action, not the result. It is told to say so rather than guess.
- **It has no tools at all.** No filesystem, no memory, no connectors, no notifications. It reads the transcript it is handed and writes prose. It cannot act on anything it finds, and it cannot look anything up.
- **Nobody is reviewed unless a link says so.** No supervision link, no review — being a child, or a member, or anything else is not what triggers it.
- **The person being reviewed does not see the report.** It is stored for their supervisors. What they *should* know — and this is a matter for the household, not the software — is that their account is supervised at all.
- **A quiet report is the normal one.** The agent is told to report what a careful adult would want to know and could act on: distress, someone pressuring or approaching them, a risk to their safety, money, a pattern repeating across days. It is told *not* to report swearing, sulking, secrecy, embarrassment, awkward questions asked out of curiosity, or homework they wanted done for them. Most nights it should conclude there is nothing to report, and that is the system working — a review that passed on everything would be read once and ignored afterwards.

The report is kept where the supervisors can read it rather than in the reviewed person's own space, and it names its window, so two reports never cover the same evening twice.

## Why a run can be missing

Users are handled one at a time, and a user is **skipped** if they have not logged in since the server last restarted.

This is not a fault, it is how the encryption works: a person's data is unreadable until they log in and their password unlocks it. Until that happens there is nothing to read and nowhere to write. Nothing is lost — events keep accumulating, and the first run after they log in picks up everything waiting.

So if someone asks "why didn't it tell me about that email from this morning?", the first thing to check is whether they had logged in at the time. The same applies to the shared memory lint: it needs an admin who has logged in since the restart.

Schedules are counted **per person from their own last run**, and they survive a restart — so a weekly pass stays weekly even on a machine that gets rebooted every few days.

The conversation review has its own version of both rules, because it is about one person but runs for another:

- The **supervised person does not need to be logged in** — provided their space is not encrypted, which is the normal setup for an account somebody else looks after. Without that, nothing could ever run at four in the morning. A supervised person who *has* an encrypted space is reviewed only while they are logged in, and there is no way around that: no password, no key, no reading it.
- **At least one of their supervisors must be logged in**, because the review has to run somewhere. If none is, the review waits, and the next one covers the whole stretch that was missed instead of losing it.
- If the machine was off at the scheduled hour, the review runs at the next start and covers everything since the last one — three days off means one report covering three days, not three missing reports.

## The System agents page

Sidebar → **System agents**. There is one tab per agent, plus **All**. A tab holds that agent's description, its settings (admin only), and its run history — because "why did this do nothing last night?" is usually half a settings question and half a log question.

Each row is one run, newest first:

- **Started** and **Duration**.
- **Status** — completed, failed, or still running.
- **Result** — the agent's own counters (events looked at, notes read, notifications sent), or the error if it failed.

Clicking a row opens the conversation the run happened in, for anyone who wants to see the reasoning.

A run appears **only when there was something to look at**. Long gaps mean quiet connectors or an untouched memory store, not a broken agent.

### Running one now

Each agent's tab has a **Run now** button, next to its description. It starts one pass immediately, for **you**, without waiting for the schedule — useful after tidying up a lot of notes, or when someone wants to see what an agent actually does instead of reading about it.

- It runs **as you**, over your own things, and reports to you. The shared memory lint is the interesting case: pressed by a member it reads the same shared store the admin's nightly pass reads, and the report simply goes to the member who asked instead of the admin.
- **"Nothing to look at right now"** is a normal answer and arrives straight away — an empty memory store or an empty event queue starts no run at all, exactly like a scheduled pass that finds nothing.
- The pass then runs in the background: the row appears in the log below as *running*, and the notification arrives when it is done. Leaving the page does not stop it.
- Pressing it again while it is still going does nothing — one pass of one agent at a time, so a manual run and the nightly one can never collide.
- Running it by hand **counts as that person's pass**: the next scheduled one is then a full interval away, rather than arriving an hour later.
- An agent an admin has switched **off** cannot be started this way. The schedule is a question of *when*, which the button answers; enabled is a question of *whether*, which stays the admin's.

The conversation review has no button: it is about somebody else and picks its own subjects, so "run it for me" would not mean anything.

**The run history is personal.** Each run is written into that user's own encrypted database, so every user — the admin included — sees their own runs and nobody else's. There is no instance-wide view.

## What the admin can change

Each agent's tab carries the same three settings, visible only to an admin:

- **Enabled** — turns that agent on or off for the whole instance, for everyone.
- **Interval** — how long between passes for each person. Event triage is in minutes, the lints in days. The conversation review has **Run at (hour)** instead: it runs once a day, after that hour, local time — 4am by default, so the report is waiting in the morning.
- **Security group** — which tools the agent may use during a run. It is re-checked against each user's own role: if their role does not allow that group, their run uses their role's default group instead. Nobody's background agent gets more access than their role would give them. (The conversation review ignores this in practice: it is given no tools whatsoever, so there is nothing for a group to permit.)

For the first three there is no per-user on/off switch: if the agent is enabled, it runs for everyone who has logged in. The conversation review is the opposite — it runs for **nobody** until an admin creates a supervision link, and that link is what turns it on for one person.
