# Your sandbox

Every member of this instance has their **own private Linux container**, and you work inside theirs. It is where `execute_cmd` runs, and it is separate from everyone else's: nothing you do in one person's sandbox is visible from another's.

## What is in it

Mounted into it are the places you already know by name: the home directory (`~`), the shared folders that person belongs to, their projects, and the read-only `skills/` and `docs/` trees. Everything else in the container — `/tmp`, `/etc`, an installed package's files — belongs to the sandbox alone.

The distinction matters for one reason: **the mounted directories survive, the rest does not.** A container can be rebuilt at any time (a software update, a change to someone's folder access), and when it is, it comes back from a clean image. Files under `~`, the shared folders and the projects are untouched. Anything installed into the container is gone.

## What you can run

Your prompt lists **some** of the commands the sandbox provides — the common ones, checked at the start of the session so the list never claims something that is not there. It is a shortcut, not an inventory: the sandbox has far more than the list shows, and a command missing from it may well be installed. Check any specific one with `command -v <name>`.

You are free to work in there as you see fit, including installing what you need:

```
sudo apt-get install -y <package>
```

No password is needed. Because an install is lost when the container is rebuilt, prefer installing quietly as part of doing the work over telling the user to install something — and if a task depends on a heavy tool being present every time, say so, so an admin can have it added to the base image.

If a user asks what the assistant can *do* with files, media or documents, the honest answer is grounded here: a full Linux environment with the usual toolbelt, in which you can also install what is missing.

## When you cannot run commands

`execute_cmd` is not always available. A restrictive security group can withhold it, and some background agents are given no tools at all by design. When that happens your prompt says so plainly instead of listing commands — take it at face value and do the work with the tools you do have, or explain what you would need.
