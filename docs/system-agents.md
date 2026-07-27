# System agents

A **system agent** is an assistant that runs in the background on someone's behalf, without being asked. Nobody starts it and nobody is waiting for its answer: it wakes up on a schedule, looks at something, and gets in touch only if there is a reason to.

Today there is exactly one: **TIC**.

## What TIC does

Connectors (Gmail, a calendar, WhatsApp…) push events into the system as they happen — a new message arrives, a meeting is moved. Those events pile up quietly; nothing interrupts the user.

Every so often TIC wakes up and reads the batch that accumulated since last time. For each event it decides whether it is worth the interruption, using what it knows about that person from their private memory: who matters to them, what they are working on, what they have said they want to be told about. Events that pass become notifications in their Inbox. Events that don't are simply marked as seen — a newsletter or a group chat with nothing relevant in it produces nothing.

An empty run is a correct run. TIC is not supposed to find something every time.

TIC only **reads and reports**. It never replies to a message, moves a calendar event, or changes anything — if an event needs an action, it says so in the notification and the user decides.

## It runs per person, and only sees one person's things

This is the part worth being precise about, because people ask.

TIC runs separately for each user. When it runs for someone, it reads only the events from **that person's own connectors**, consults only **their private memory**, and delivers notifications only to **them**. Two people on the same instance never see each other's events through TIC, and the admin does not see anyone's.

The same applies to the record of what it did: each run is written into that user's own encrypted database, so the run history on the **System agents** page is personal — every user, admin included, sees their own and nobody else's.

## Why a run can be missing

Users are handled one at a time, and a user is **skipped** if they have not logged in since the server last restarted.

This is not a fault, it is how the encryption works: a person's data is unreadable until they log in and their password unlocks it. Until that happens there is nothing for TIC to read and nowhere for it to write. Their events are not lost — they keep accumulating, and the first run after they log in picks up everything waiting.

So if someone asks "why didn't it tell me about that email from this morning?", the first thing to check is whether they had logged in at the time.

## The System agents page

Sidebar → **System agents**. One row per run, newest first:

- **Agent** — which system agent ran (`tic`).
- **Started** and **Duration**.
- **Status** — completed, failed, or still running.
- **Result** — how many events it looked at and how many notifications it produced, or the error if it failed.

Clicking a row opens the conversation the run happened in, for anyone who wants to see the reasoning.

A run appears **only when there were events to look at**. Long gaps between rows mean quiet connectors, not a broken agent — if there is nothing new, TIC does no work and records nothing.

## What the admin can change

On the admin's Config page (see [settings.md](settings.md)), under **TIC Agent**:

- **Enabled** — turns TIC on or off for the whole instance, for everyone.
- **Check Interval (minutes)** — how often a pass over all users starts.
- **Security Group** — which tools TIC may use during a run. This is re-checked against each user's own role: if their role does not allow that group, their run uses their role's default group instead. Nobody's background agent gets more access than their role would give them.

There is no per-user on/off switch: if TIC is enabled, it runs for everyone who has logged in.
