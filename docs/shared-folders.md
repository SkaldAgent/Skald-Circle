# Shared folders

A **shared folder** is a folder on the server that several members of the group can use together — a single place for documents everybody needs, instead of each person keeping their own copy and asking for the latest version by hand.

Examples: a shared recipe collection, the household's documents (bills, contracts, manuals), a folder where members drop files for the whole group to see.

Shared folders are **managed by an admin**: an admin creates them, decides who is a member and what each member may do in them. There is **no owner** — the person who created a folder has no special rights over it; the admin can change or remove anyone, themselves included.

Two things shared folders are *not*, so expectations stay right:

- They have **no chat of their own** — the files are shared, not a conversation. (That is what Projects are for; see [Shared folders vs Projects](#shared-folders-vs-projects) below.)
- They have **no file explorer page** in the web app. Members work with the files through the assistant, and open individual files in the file viewer when the assistant shows them (see [Working with the files](#working-with-the-files)).

## Creating a folder (admin)

1. Open **Shared Folders** in the sidebar (admin only; members do not see this page).
2. Click **New Folder** and fill in:
   - **Name** — a short, simple name with no spaces or punctuation: `recipes`, `documents`, `holiday-pics`. The name becomes the folder's path, so it must be a single word (no `/`, `\`, `.` or `..`). It **cannot be changed later** — pick carefully.
   - **Description** — what the folder is for, in plain words. This is not decoration: it is what the assistant reads to understand the folder (see [What the assistant knows](#what-the-assistant-knows)). A good description: *"Family documents: bills, contracts, manuals — everyone can read, only Marta writes."*
3. Save. The folder is created on the server immediately.

There is no step 4: a folder starts **empty**, with **no members** — even the admin is not a member until added. Add members next (see below).

## Who can see it: members

A folder is visible only to its members. The admin adds members from the folder's row on the Shared Folders page, choosing each person's access level:

- **Read** — can open and read the files, and ask the assistant to work with them. Cannot create, edit or delete anything.
- **Read & write** — everything Read gives, plus creating, editing and deleting files (directly or through the assistant).

Two things worth knowing about membership:

- **Changes apply immediately.** Adding, removing or changing a member takes effect right away — the other person does not need to log out and back in.
- **Removing access is the only way to take files away** — and it works: a person who is no longer a member can no longer see the folder, its files, or ask the assistant about them.

## Working with the files

There is no file explorer for shared folders — no grid of files, no upload button. The files live on the server, and members reach them through the assistant:

- **Ask the assistant** — "what's in the recipes folder?", "add this note to documents", "send me the manual for the boiler". The assistant knows which folders you belong to, can list their contents, open and search files, and — if you have read & write — create and edit them.
- **Open a file** — when the assistant shows you a file from a shared folder, it opens in the usual file viewer (Markdown rendered, images, PDFs, syntax-colored code, text), exactly like any other file. You can read it there; editing in the viewer is available if you have read & write access.

A practical consequence: if a member wants a file *from* a shared folder, the assistant is the way to get it — there is no download button on the folder itself. (An admin can of course reach the folder directly on the server, but members should not need to.)

## What the assistant knows

At the start of every conversation, the assistant sees a table of the shared folders you belong to, with four columns:

| Path | Access | Shared with | Description |
|------|--------|-------------|-------------|
| `shared/recipes` | read-write | — | Family recipes, everyone can add |
| `shared/documents` | read-only | Anna, Luca | Bills and contracts |

So the assistant knows the folder exists, **your** access level in it, **who else** can see it, and what the admin wrote in the description — and nothing else. It does not read the files on its own: it looks inside only when you ask, and it may ask you to confirm that something belongs in a shared folder before putting it there (see below).

This is why the description matters: it is the folder's only explanation, and a folder with an empty description is a folder the assistant cannot reason about. If you are an admin and a shared folder has no description, editing it (Shared Folders → the folder → edit) is the most useful thing you can do with it.

## The approval cards

The assistant never changes a shared folder on its own initiative — and even on your request, **reading and writing in a shared folder asks for your confirmation first**, as a small card you answer in the chat (or in the Inbox, or from your phone, depending on where you are talking).

This is deliberate and it applies to *everyone*, the admin included:

- **Reads ask too**, not just writes. A shared folder may contain things other members wrote, and the system does not assume you want the assistant browsing it freely.
- The card shows exactly what the assistant wants to do — open this file, create that one, change this line — with **Approve** and **Deny** buttons. Answering is the whole flow; you do not need to do anything else.
- **If you deny**, the assistant simply does not do it and moves on — nothing is forced.

If this feels like a lot of questions, remember the trade-off is the point: shared folders are the one place where one person's words become *everyone's* files, so every step is a conscious one. (Projects work differently — see below.)

## Shared folders vs Projects

The two features look similar — a shared place for files with per-member access — but they solve different problems:

| | Shared folder | Project |
|---|---|---|
| Who manages it | An admin (no owner; anyone can be removed) | The owner (a member) and read & write members |
| Where it lives in the chat | No chat of its own | Its own conversation with the assistant (`project-{id}`), plus extra tabs |
| Files | No explorer page; work through the assistant | A live file explorer with upload, rename, delete, ZIP download |
| Assistant's access | Every read/write asks for confirmation | Reads and writes are frictionless (only the folder's membership limits them) |
| Typical use | A place to *keep* shared documents | A place to *work together* on something |

When to use which, in one line: if the point is "we have documents here", a shared folder; if the point is "we are working on something here", a project. And the two combine naturally — a project's chat can refer to shared folders, and the assistant can copy files between them if you have the right access.

## Notes

- **The name never changes.** A shared folder cannot be renamed (there is no rename button). To "rename" one, an admin creates a new folder and moves the files into it — which changes the folder's path and requires re-adding members.
- **Deleting a folder does not delete the files.** When an admin deletes a shared folder, only the sharing is removed: the folder stays on the server, and an admin can remove it by hand if that is really intended. Say this plainly if a user believes deleting the folder destroyed its contents.
- **The Shared Folders page is admin-only.** Members never see it, and a member cannot create folders, add members, or change access levels. Direct them to the admin rather than trying to do any of it on their behalf.
- **The assistant can copy files *into* a shared folder** (with your approval) — a good way to publish something from your private space to the group. The reverse works too, if you are a member.
- **A description is not a substitute for access.** Writing "everyone may read this" in the description tells the assistant the intent, but membership is still decided by the admin on the Shared Folders page — the assistant cannot grant access, only tell you who currently has it.
