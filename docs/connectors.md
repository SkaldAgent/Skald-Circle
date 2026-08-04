# Connectors

A **connector** gives you tools that reach outside this instance — a mailbox, a calendar, a web search, a messaging account. Internally they are MCP servers, but nobody calls them that in the interface: the sidebar entry is **Connectors**, so use that word when talking to a user.

Before saying anything about which connectors exist or work, call `list_items({"type": "mcp"})`. It reports the real state for the person you are talking to, and its answer beats any assumption — including anything written below.

## Two kinds, and the difference is about whose account

- **Shared connectors** run centrally on the server, under credentials the admin owns (web search is the usual example). They are not tied to anyone's account, and everyone granted one gets the same thing.
- **Per-user connectors** run inside that person's own private container and are bound to *their* account. Gmail means their mailbox, never another member's. This is why setting one up needs them to sign in personally: an admin cannot do it on their behalf, and a grant only authorizes them to set it up.

## Who has one

Installing a connector hands it to everyone straight away, and the admin then removes it from whoever should not have it — the full rules, including the role switch that keeps children out of the automatic hand-out, are in [access.md](access.md).

Being granted a per-user connector is not the same as having it working: the person still has to activate it and sign in.

## Setting one up

All of it happens in the web UI, on the **Connectors** page in the sidebar. There is no way to do it by asking the assistant, and no tool for it — if someone asks you to enable, configure or activate a connector, explain the steps and let them do it.

1. Open **Connectors** and pick one from the list.
2. Activate it. Some connectors ask for a value (an API key, a URL); the form says which.
3. Finish the sign-in, if it needs one. Two shapes exist:
   - **Sign-in with an account** (Gmail, Calendar): a button opens the provider's consent page in a browser, which ends by showing a code. Paste that code back into the connector's page. The round trip is deliberate — this instance has no public address for the provider to call back to.
   - **Device pairing** (WhatsApp): the connector's page shows a QR code to scan with the phone app, the same way that app pairs any other device.

An admin has one extra job: the catalogue itself. New connectors are installed from the **Marketplace** (reached from the Add-connector menu on the Connectors page), and account-based sign-ins need the provider's credentials entered once, under **Sign-in providers**.

## When a connector does not work

`list_items({"type": "mcp"})` puts each one in a bucket and says what to do. The states worth recognising:

- **Waiting on a sign-in** — activated, but step 3 above was never finished, so there is no stored credential. Nothing will work until the person completes it.
- **Not running** — activated and configured, but its process is not up. Signing out and back in usually restarts it.
- **Available, never activated** — the person is allowed to have it but has not set it up yet.

A connector that is missing from the report entirely was never granted. That is the admin's call, so the answer is to ask them, not to look for a workaround.

## Using one

A connector's tools are not loaded until you ask for them: `activate_tools(["<id>"])` loads them for the rest of the session, and they are then called as `mcp__<id>__<tool>`. You do not need to explain any of this to the user — to them, the connector either works or does not.

See also: [access.md](access.md) for grants and roles, and [index.md](index.md) for the rest of the documentation.
