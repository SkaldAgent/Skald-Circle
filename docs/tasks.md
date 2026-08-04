# Background tasks (work that keeps running while you talk)

Some requests take minutes rather than seconds — reading a long document, searching the web thoroughly, crunching a folder of files. For those, the assistant can hand the work to a **background task**: a second agent that goes off and does it while the conversation carries on. The user does not have to sit and wait, and can keep asking about other things.

This is different from the two neighbouring things it is easy to confuse it with:

- A **sub-task** (the ordinary kind) runs *inside* the current answer. The conversation waits for it, and its progress is visible in the transcript as it happens.
- A **scheduled job** (a cron job) runs at a time of day, on repeat, and belongs to nobody's conversation. Those live on the **Tasks** page and report to the home chat.
- A **background task** belongs to the conversation that started it, and comes back to it.

## Seeing them: the strip above the message box

While a background task is running, a small strip appears **just above the composer**, in the desktop chat and the mobile one alike. One line per task: its title, which agent is doing it, and how long it has been going.

- **Clicking a task opens its own page**, where its work is shown live — the same view used for any background agent. This is the answer to "what is it actually doing?", which the chat itself cannot show: the task is a separate conversation.
- **The ■ button stops a task.** It stops there and then; whatever it had done so far is not thrown away, but it is incomplete, and the assistant is told so.
- The strip survives a page reload. It shows what is running *now*, so a task that finished while the browser was closed will not be there — but its result will be in the conversation, which is the better place to read it.

## How a task comes back

**Every** background task ends up back in the conversation that started it. There is no case where the user has to go looking for the outcome:

- **It succeeded** — its answer arrives as a message, and the assistant carries on from there, usually with a summary.
- **It failed** — the conversation is told it failed and why, together with whatever the task managed to say before it broke. The assistant should treat this as a real result and say so plainly, not quietly ignore it.
- **It was stopped** by the user — the conversation is told the work is incomplete. It must not be presented as if it had finished.

A finished task's line disappears from the strip after a few seconds. A **failed** one stays, so the reason can be read, until it is dismissed with the ✕.

## When a task needs the user

A background task can hit something it is not allowed to do on its own — running a command, writing outside its own area — or it can simply need to ask a question. Because the task is a separate conversation, it cannot interrupt the chat the way the assistant does mid-answer. Instead, **a card appears at the top of the task strip**, above the message box: the approval to grant, or the question to answer, labelled with the task that is asking.

- **It waits.** A task that has asked for something is stopped until it gets an answer. Nothing else of it moves in the meantime.
- **One at a time.** If several tasks are asking, the card shows the first and says how many are behind it (*1 of 3*); answering one brings up the next.
- **It can be closed.** The **✕** in the card's top-left corner puts it away — it does *not* approve or reject anything. The request is still pending, the task is still waiting, and it can be dealt with from the **Inbox** (sidebar → Inbox) whenever the user is ready. The strip keeps a small "waiting in the Inbox" pointer while any closed request is still outstanding.
- **Anywhere works.** The same request appears in the Inbox and, if the mobile app is paired, on the phone. Answering it in any one of those places settles it everywhere; the card disappears on its own.

The chat's *own* approvals are unaffected by all this — when the assistant itself needs permission mid-answer, the card still appears inline, in the transcript, where the work is happening.

## When a user asks about them

Common questions and the honest answers:

- *"Is it still running?"* — the strip is the answer; if the strip is empty, nothing of theirs is running.
- *"What is it doing?"* — click the task's line.
- *"It has been going for ages."* — a task has no time limit; stopping it with ■ is always available, and stopping is not the same as failing.
- *"Where did the result go?"* — into this conversation, always. If it is not there yet, the task has not finished.
- *"It's stuck."* — check whether it is asking for something: an approval or a question waiting at the top of the strip, or in the Inbox if the card was closed earlier.
- *"Show me everything that ever ran."* — the **Tasks** page (sidebar → Tasks) has the full history, including scheduled jobs; the strip only covers the current conversation.
