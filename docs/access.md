# Who can use what: plugins, connectors and roles

Plugins and connectors are installed once by the admin, for the whole instance. Whether a *given person* can use one is a separate question, answered by a **grant**.

## The default is open

When the admin installs something new — a plugin, a globally-shared connector, or a connector from the marketplace — it is **handed to everyone straight away**. The admin's remaining job is to take it away from whoever should not have it, not to hand it out one person at a time.

The same applies in the other direction: a **new user** starts out holding everything the household already uses, so a new member does not arrive to an empty account.

Two things are worth knowing about how this works, because they explain behaviour that would otherwise look surprising:

- **It applies at installation, not at every switch-on.** Disabling a plugin and enabling it again does *not* re-grant it to people the admin removed it from. Their decision stands.
- **Removing access is normal and expected.** Access is taken away per person, from that person's own page: sidebar → **Users** → click the person → the **Connectors** and **Plugins** sections. Unticking a box there is the intended way to say "not for you", and nothing later puts it back.

Admins never need a grant: they can use every enabled plugin and connector by construction.

## Roles decide who is included

Whether a role's members are included in that automatic hand-out is a property of the **role**, set in the role editor (sidebar → Roles → edit a role → **New plugins and connectors**):

- **On** (the default) — anything the admin installs reaches these people immediately. This is what an adult member of the household normally wants.
- **Off** — these people only ever get what the admin explicitly gives them, one at a time, from their own page. The **Children** role ships with this switched off, and it is the reason the setting exists: a connector installed late at night should not silently become available to a child.

Turning the switch on or off changes nothing about access that has already been granted — it only decides what happens the next time something is installed, or the next time a person is added to that role.

Two consequences worth mentioning to a user who runs into them:

- Someone whose role has the switch **off** will see nothing new appear, ever, until the admin ticks their box. That is working as intended, not a bug.
- Changing a person's role does **not** retroactively hand them everything installed so far. If a child is moved to an adult role, the admin still ticks the boxes on that person's page once.

## What a grant actually does

The three things being granted are not the same, and the difference matters when explaining it:

- **A plugin grant** makes the plugin visible and usable for that person — its sidebar page, its tools, its channel (Telegram checks the grant on every incoming message). It is re-checked continuously, so removing it takes effect immediately.
- **A shared connector grant** (one the admin runs centrally, e.g. web search) puts that connector's tools in that person's assistant.
- **A per-user connector grant** (e.g. Gmail, WhatsApp) only authorizes the person to *set it up* — they still have to sign in with their own account. Nobody ever uses somebody else's credentials through a grant.

See also: [index.md](index.md) for the plugin list, and each plugin's own page under [`plugins/`](plugins/).
