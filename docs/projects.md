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

## The Files tab

A file explorer rooted at the project folder:

- The **breadcrumb** on top shows where you are, relative to the project root (`/`, then `/folder`, `/folder/subfolder`). Click any segment to jump back.
- Files and folders are listed as a table: icon, name, creation date, last-modified date, size.
- Click a **file** to open it in the file viewer (Markdown rendered, images, PDFs, text…).
- Click a **folder** to navigate into it.
- The listing **updates by itself**: if another member or the assistant creates, renames or deletes a file while you're looking at a folder, the change appears within a second — no refresh needed.

If you have write access you can also, from the toolbar or each row:

- **New folder** — create a subfolder in the current location.
- **Upload** — send files from your device into the current folder (or just drag & drop them onto the list).
- **Rename** and **Delete** — from the icons on each row. Deleting a folder removes everything inside it, after a confirmation.

Read-only members see the same explorer and can open every file, but the write actions are hidden (and refused by the server anyway).

## The Sharing tab

Lists every member with their access level. The owner and any read & write member can:

- **Add a member** — pick a person and their access: *Read* (browse and open files only) or *Read & write* (can also create, edit, delete and share).
- **Change access** or **remove** a member (the owner can't be removed).

Access changes apply immediately — no need for the other person to log out.

## Notes

- A private project is simply a project with one member (you). Share it later whenever you want.
- Renaming a project does not move its folder, so links and the assistant's context keep working.
- Deleting a project removes its folder for everyone — there is no undo.
