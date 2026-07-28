# ComfyUI

- **Plugin id:** `comfyui`
- **Category:** Image generation, local
- **Runs:** talks to a separately-running ComfyUI server over HTTP (default `http://localhost:8188`)

## What it does

Turns a locally running [ComfyUI](https://github.com/comfyanonymous/ComfyUI) installation into one or more image-generation models the assistant can call. Every JSON workflow file dropped into a watched folder becomes its own selectable model — so a user can have separate "photo-realistic", "anime", "upscale" etc. workflows, each showing up independently.

The plugin polls the ComfyUI server every 5 seconds. If it's offline, every model built from it disappears until it comes back; if a workflow file is added, edited, or removed, the model list updates live — no restart needed.

## Requirements

- ComfyUI installed and running somewhere reachable from this machine (its own GPU-heavy install, not something this app manages).
- At least one workflow exported from ComfyUI as a JSON API file, containing a `CLIPTextEncode` node (for text-to-image) and/or an image-loading node (for image-to-image). Advanced workflows can add a `_personal_agent` metadata block inside the JSON to control exactly which node/field receives the prompt, negative prompt, width/height/steps, and input image — without that block the plugin auto-detects the first `CLIPTextEncode` node.

## Enabling & configuring (admin)

1. Plugins page → **ComfyUI** → enable, then **Configure**.
2. Fields:
   - **`base_url`** (default `http://localhost:8188`) — where ComfyUI's API is listening.
   - **`workflows_dir`** (default `data/comfyui/workflows`) — folder to watch for `.json` workflow files. Created automatically if missing.
   - **`default_negative`** — an optional negative prompt applied to every workflow that has a negative-prompt node.
3. Put workflow JSON files in the workflows folder. Each becomes a model named after the file (or the `name` given in its `_personal_agent` block) in the **Image generation** section of the Models hub — no separate "add provider" step, since this plugin registers its models directly.

## Notes

- A single generation can take up to 5 minutes before the plugin times out and reports an error.
- Image-to-image is supported when the workflow declares an `input_image_node` in `_personal_agent`.
- If ComfyUI is unreachable, tell the user to start their ComfyUI server — there is nothing to fix in this app's configuration in that case.
