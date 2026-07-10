# ComfyUI Workflow Format — Agent Guide

Guide for receiving a ComfyUI workflow file (e.g. via Telegram attachment),
understanding it, adding the required metadata, and saving it as an image
generation provider.

---

## JSON API Format Structure

When exporting a workflow from ComfyUI with "Save (API Format)", the result is
a JSON where every **numeric** key is a pipeline node:

```json
{
  "3":  { "class_type": "KSampler",           "inputs": { "steps": 20, "cfg": 7, "seed": 42, ... } },
  "4":  { "class_type": "CheckpointLoaderSimple", "inputs": { "ckpt_name": "v1-5-pruned.safetensors" } },
  "6":  { "class_type": "CLIPTextEncode",     "inputs": { "text": "", "clip": ["4", 1] } },
  "7":  { "class_type": "CLIPTextEncode",     "inputs": { "text": "", "clip": ["4", 1] } },
  "8":  { "class_type": "EmptyLatentImage",   "inputs": { "width": 512, "height": 512, "batch_size": 1 } },
  "9":  { "class_type": "SaveImage",          "inputs": { "filename_prefix": "ComfyUI", "images": ["10", 0] } },
  "10": { "class_type": "VAEDecode",          "inputs": { "samples": ["3", 0], "vae": ["4", 2] } }
}
```

**Key rules:**
- Numeric keys are node IDs. They need not be consecutive.
- **Non-numeric** keys (e.g. `_personal_agent`) are ignored by ComfyUI
  and used only by the plugin.
- Array values in `inputs` (e.g. `["4", 1]`) are links to other nodes:
  `[node_id, output_slot]`.

---

## Relevant Nodes

| `class_type` | Field to modify | Description |
|---|---|---|
| `CLIPTextEncode` | `inputs.text` | Text prompt (positive or negative) |
| `CLIPTextEncodeSD3` | `inputs.clip_l`, `inputs.clip_g`, `inputs.t5xxl` | SD3.5 text prompt — all three fields must be populated |
| `KSampler` | `inputs.steps` | Number of diffusion steps |
| `KSampler` | `inputs.cfg` | CFG scale (creativity vs. prompt adherence) |
| `KSampler` | `inputs.seed` | Seed for reproducibility |
| `EmptyLatentImage` | `inputs.width` | Image width in pixels |
| `EmptyLatentImage` | `inputs.height` | Image height in pixels |
| `CheckpointLoaderSimple` | `inputs.ckpt_name` | SD model name to use |
| `LoraLoader` | `inputs.lora_name` | LoRA to apply |
| `SaveImage` | `inputs.filename_prefix` | Output filename prefix |

---

## The `_personal_agent` Block

Add this as a non-numeric key to the JSON to configure the provider.

### Full Schema

```json
"_personal_agent": {
  "name":                        "Workflow Name",
  "description":                 "Description for the agent: style, format, ideal use cases.",
  "prompt_node":                 "6",
  "negative_prompt_node":        "7",
  "prompt_field":                "clip_l",
  "prompt_field_extra":          ["clip_g", "t5xxl"],
  "negative_prompt_field":       "clip_l",
  "negative_prompt_field_extra": ["clip_g", "t5xxl"],
  "extra_params": {
    "width_node":  "8",
    "height_node": "8",
    "steps_node":  "3"
  }
}
```

| Field | Required | Notes |
| ----- | :------: | ----- |
| `name` | ✓ | Name shown in the provider listing |
| `description` | — | Free text for the agent: style, default dimensions, use cases |
| `prompt_node` | ✓ | ID of the node for the positive prompt (`CLIPTextEncode` or `CLIPTextEncodeSD3`) |
| `negative_prompt_node` | — | ID of the node for the negative prompt |
| `prompt_field` | — | Input field to inject the prompt into. Default: `"text"`. For SD3.5: `"clip_l"` |
| `prompt_field_extra` | — | Additional input fields to copy the prompt into. For SD3.5: `["clip_g", "t5xxl"]` |
| `negative_prompt_field` | — | Input field for the negative prompt. Default: `"text"` |
| `negative_prompt_field_extra` | — | Additional input fields for the negative prompt |
| `extra_params.width_node` | — | ID of the node with `inputs.width` (usually `EmptyLatentImage`) |
| `extra_params.height_node` | — | ID of the node with `inputs.height` |
| `extra_params.steps_node` | — | ID of the node with `inputs.steps` (usually `KSampler`) |

If `prompt_node` is omitted, the plugin heuristically picks the first
`CLIPTextEncode` or `CLIPTextEncodeSD3` node found (ascending numeric ID order).

---

## Identifying the Correct Nodes

To find the right node IDs by reading the JSON:

1. **Positive prompt** — find nodes with `"class_type": "CLIPTextEncode"` or
   `"CLIPTextEncodeSD3"`. There are usually two: one for the positive prompt
   (empty or descriptive text) and one for the negative. Conventionally the
   positive one has the lower ID.

2. **Dimensions** — find `"class_type": "EmptyLatentImage"`. Read
   `inputs.width` and `inputs.height` to know the workflow's default dimensions.

3. **Steps** — find `"class_type": "KSampler"`. The `inputs.steps` field is
   the number of diffusion steps.

### Example: reading defaults for `extra_params_schema`

Given the node:
```json
"8": { "class_type": "EmptyLatentImage", "inputs": { "width": 768, "height": 1024 } }
```
The plugin will automatically generate:
```json
"extra_params_schema": {
  "properties": {
    "width":  { "type": "integer", "default": 768  },
    "height": { "type": "integer", "default": 1024 }
  }
}
```

---

## Recommended Editing Workflow

1. **Receive the file** — Telegram attachment, web upload, or local path.

2. **Read the JSON** and identify:
   - The `CLIPTextEncode` node for the positive prompt (lowest ID among those present).
   - The `CLIPTextEncode` node for the negative prompt (if present).
   - The `EmptyLatentImage` node for dimensions.
   - The `KSampler` node for steps.

3. **Add `_personal_agent`** with the discovered node IDs and a meaningful
   description. Example for a landscape 1024×512 workflow:

   ```json
   "_personal_agent": {
     "name": "Landscape XL",
     "description": "Landscapes and horizontal scenes. Default 1024x512. Great for backgrounds and scenery.",
     "prompt_node": "6",
     "negative_prompt_node": "7",
     "extra_params": {
       "width_node":  "8",
       "height_node": "8",
       "steps_node":  "3"
     }
   }
   ```

4. **Save to** `data/comfyui/workflows/<name>.json`.
   The filename becomes the provider ID: `landscape-xl.json` →
   provider `comfyui-landscape-xl`.

5. **The watcher detects the file within 5 s** and registers the provider.
   Verifiable by calling `image_generate_providers_list`.

---

## Notes on Workflows Without `_personal_agent`

If the file does not contain a `_personal_agent` block, the plugin:
- Uses the filename as the provider name.
- Heuristically searches for the first `CLIPTextEncode` or `CLIPTextEncodeSD3` node for the prompt.
- Registers the provider without `description` or `extra_params_schema`.
- If no `CLIPTextEncode` or `CLIPTextEncodeSD3` node is found, **skips the file** with a warning.

Adding `_personal_agent` is always preferred to give the agent the context
needed to pick the right provider.
