# Projects

A **project** is a shared workspace: a folder on the server plus its own chat with the assistant. Members of the group can work on the same files — directly, or by asking the assistant in the project chat — without giving anyone access to their private home folder.

Examples: a household budget, a holiday plan, a shared recipe collection, a small work document base.

## Creating a project

1. Open **Projects** in the sidebar.
2. Click **New Project**, give it a name and an optional description, save.

You become the project's owner. Only you can delete the project; everything else (editing the description, sharing) can also be done by members you grant read & write access.

## The project page

Opening a project shows its page, with two tabs (the current tab is part of the address, so you can bookmark or share the link):

- **Files** — the project's file explorer (see below).
- **Sharing** — who can access the project.

The header also has an **Open chat** button: it opens the project's conversation with the assistant. The assistant already knows the project folder and works directly inside it — creating documents, searching, summarizing. Each member has their **own private** conversation about the project; only the files are shared.

The conversation opens as a **tab** in the chat panel, next to the General one. **Open chat** always takes you back to the project's own conversation, with everything you had already said in it — it never starts a fresh one.

Those tabs stay open: they survive a page reload, and because they are saved to your account rather than to the browser, you find the same ones when you sign in from another device. Closing a tab only removes it from the bar — the conversation itself is kept, and reopening the project brings it back with its history. The General tab is always there and cannot be closed.

**Working on two things at once.** The **+** button at the end of the tab bar opens one more chat, either general or on a project you belong to. It is a separate conversation with its own history: the assistant in it knows nothing about what you are saying in the other tabs, which is the point — you can leave a long piece of work open in one tab and ask something unrelated in another without mixing them up. On a project, the extra chat knows the project's folder and members just like the main one.

Two differences between a project's own chat and an extra one are worth knowing. Notifications from the assistant, results of background tasks and messages arriving from a connected chat app are delivered to the project's own conversation (and General for everything else) — never to an extra tab. And **Open chat** always lands on the project's own conversation, so an extra chat is reached only from its tab.

**Renaming.** Double-click a tab to give it a name, then press Enter. Clearing the box restores the automatic name.

## The Files tab

A file explorer rooted at the project folder:

- The **breadcrumb** on top shows where you are, relative to the project root (`/`, then `/folder`, `/folder/subfolder`). Click any segment to jump back.
- Files and folders are listed as a table: icon, name, creation date, last-modified date, size.
- Click a **file** to open it in the file viewer (Markdown rendered, images, PDFs, text…).
- Click a **folder** to navigate into it.
- The listing **updates by itself**: if another member or the assistant creates, renames or deletes a file while you're looking at a folder, the change appears within a second — no refresh needed.

For **Markdown** files (`.md`), if you have write access the viewer has two tabs:

- **View** — the rendered document (the default).
- **Edit** — edit the Markdown source directly. Switch back to View any time to preview your changes; **Save** writes the file, **Cancel** discards.

Because the same file may be edited at the same time by another member, another of your tabs, or the assistant, saving is protected against silent overwrites: if the file changed on the server *after* you started editing, you'll see a banner — **Reload remote** (discard your edits and take the newer version), **Copy mine, then reload** (copy your edits to the clipboard, then take the remote version), or **Overwrite** (force your version). So no one's work is ever lost without you choosing.

If you have write access you can also, from the toolbar or each row:

- **New folder** — create a subfolder in the current location.
- **Upload** — send files from your device into the current folder (or just drag & drop them onto the list).
- **Rename** and **Delete** — from the icons on each row. Deleting a folder removes everything inside it, after a confirmation.

Read-only members see the same explorer and can open every file, but the write actions are hidden (and refused by the server anyway).

**Downloading.** Every member can download what they see: each row has a download icon, and the toolbar has a **Download ZIP** button that always applies to the folder you are currently browsing (at the project root, that's the whole project). A single file downloads as-is; a folder downloads as a ZIP archive, built on the fly on the server. Inside the archive the folder keeps its name, and files that are already compressed (photos, videos, PDFs, other archives) are stored as-is so the download stays fast.

## The Sharing tab

Lists every member with their access level. The owner and any read & write member can:

- **Add a member** — pick a person and their access: *Read* (browse and open files only) or *Read & write* (can also create, edit, delete and share).
- **Change access** or **remove** a member (the owner can't be removed).

Access changes apply immediately — no need for the other person to log out.

## Notes

- A private project is simply a project with one member (you). Share it later whenever you want.
- Renaming a project does not move its folder, so links and the assistant's context keep working.
- Deleting a project removes its folder for everyone — there is no undo.
