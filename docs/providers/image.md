# Image Generation

Framework for generating images from text prompts. Supports DB-backed providers (configured via UI) and plugin-registered providers (ephemeral, registered at runtime).

---

## Architecture

```text
crates/core-api/src/image_generate.rs
  — ImageGenerate trait (provider interface)
  — ImageGenerateRegistry trait (plugin write-side: register/unregister)

src/image_generate/
  mod.rs              — record types, re-exports ImageGenerate from core-api
  manager.rs          — ImageGeneratorManager (DB-backed + plugin slots, impls ImageGenerateRegistry)
  db.rs               — CRUD for image_generate_models table
  openrouter_image.rs — OpenRouterImageGenerator (chat completions + modalities)

src/tools/
  image_generate.rs   — LLM tools: image_generate_providers_list, image_generate

src/api/
  image_generate_models.rs — REST CRUD for image_generate_models
  images.rs                — GET /api/images/:id (serve generated files)
```

Two kinds of providers coexist:

| Kind | Source | Example |
| ---- | ------ | ------- |
| **DB-backed** | Rows in `image_generate_models`, built from `llm_providers` credentials | OpenRouter `x-ai/grok-2-vision` |
| **Plugin-registered** | Ephemeral — registered at runtime by plugins | future: `StableDiffusionPlugin` |

Plugin-registered providers take precedence over DB-backed ones in `get()`.

---

## Traits (crates/core-api)

```rust
// core_api::image_generate
#[async_trait]
pub trait ImageGenerate: Send + Sync {
    fn id(&self)   -> &str;
    fn name(&self) -> &str;
    async fn generate(&self, prompt: &str) -> Result<Vec<u8>>;  // raw PNG bytes
}

/// Write-side used by plugins to register/unregister ephemeral providers.
/// Implemented by ImageGeneratorManager in the main crate.
#[async_trait]
pub trait ImageGenerateRegistry: Send + Sync {
    async fn register(&self, provider: Arc<dyn ImageGenerate>);
    async fn unregister(&self, id: &str);
}
```

`ImageGenerateRegistry` is also available on `PluginContext` as `ctx.image_generate_registry`, so plugin crates that depend only on `core-api` can register providers without importing anything from the main crate.

---

## Manager API

```rust
// Async constructor — loads DB models on startup
ImageGeneratorManager::new(pool: Arc<SqlitePool>, data_root: impl Into<PathBuf>)
    -> Result<Arc<Self>>

// Plugin registration (ephemeral — called by a plugin's start()/stop())
image_generator_manager.register(Arc::new(provider)).await;
image_generator_manager.unregister("my_provider_id").await;

// DB-backed CRUD (called by REST API handlers)
image_generator_manager.add_model(record).await       // → Result<i64>
image_generator_manager.update_model(id, record).await
image_generator_manager.delete_model(id).await        // soft delete
image_generator_manager.get_model(id).await           // → Option<ImageGenerateModelRecord>

// Listings
image_generator_manager.list_models_info().await      // DB-backed only → Vec<ImageGenerateModelInfo>
image_generator_manager.list_all_info().await         // plugin + DB → Vec<ImageGenerateModelInfo>
image_generator_manager.list().await                  // lightweight → Vec<ImageGenerateInfo> (for LLM tool)

// Resolution
image_generator_manager.get(id).await                // → Option<Arc<dyn ImageGenerate>>

// Generation (called by image_generate tool via block_in_place)
image_generator_manager.generate(provider_id, prompt).await  // → Result<(PathBuf, String)> (path, url)

// Image storage path
image_generator_manager.images_dir()                 // → PathBuf (data/images/)
```

---

## LLM Tools

Two tools are injected per-turn when at least one provider is active (absent otherwise):

### `image_generate_providers_list`

Lists all currently active image generation providers.

```text
Parameters: (none)
Returns: JSON array of {id: string, name: string}
```

### `image_generate`

Generates an image synchronously. Blocks the tool round until the image is ready.

```text
Parameters:
  provider_id  string  (required) — ID from image_generate_providers_list
  prompt       string  (required) — text prompt

Returns: {"path": "/abs/path/data/images/<id>.png", "url": "/api/images/<id>"}
```

**Typical agent flow:**

```text
1. image_generate_providers_list()          → [{id: "grok-imagine", name: "grok-imagine"}]
2. image_generate("grok-imagine", "a red sunset") → {"path": "...", "url": "/api/images/abc123"}
```

---

## Image Storage

Generated images are written to `data/images/<random_id>.png` (relative to the working directory). The directory is created automatically on first use.

---

## REST API

### Image serving

```text
GET /api/images/:id
```

Serves the generated PNG. Returns `404` if the file does not exist, `400` for invalid IDs. No authentication (local server).

### Model management

```text
GET    /api/image-generate/models          — list all active providers (plugin + DB)
POST   /api/image-generate/models          — add a DB-backed model
GET    /api/image-generate/models/{id}     — get a model record
PUT    /api/image-generate/models/{id}     — update a model record
DELETE /api/image-generate/models/{id}     — soft-delete a model
```

**POST / PUT body:**

```json
{
  "provider_id": 1,
  "model_id": "x-ai/grok-2-vision",
  "name": "grok-imagine",
  "priority": 100
}
```

`name` becomes the `provider_id` used in the `image_generate` LLM tool. If omitted, `model_id` is used.

---

## OpenRouter provider

`OpenRouterImageGenerator` calls the OpenRouter chat completions endpoint with `modalities: ["image"]` (image-only — do **not** use `["image", "text"]`, which is for multimodal models and causes a 404 on image-only models like `grok-imagine-image-quality`). The response image is returned as a base64 data URL at `choices[0].message.images[0].image_url.url`.

To register an OpenRouter image model:
1. Add/use an existing `llm_providers` row with `type = "open_router"` and a valid API key.
2. `POST /api/image-generate/models` with that `provider_id` and the desired `model_id`.

---

## Plugin Registration

Plugin crates depend only on `core-api` — no reference to the main crate needed.

```rust
// In crates/plugin-foo/Cargo.toml:
// core-api = { path = "../core-api" }

use core_api::image_generate::ImageGenerate;

struct MyImageGenerator { /* ... */ }

#[async_trait]
impl ImageGenerate for MyImageGenerator {
    fn id(&self)   -> &str { "my_generator" }
    fn name(&self) -> &str { "My Generator" }
    async fn generate(&self, prompt: &str) -> Result<Vec<u8>> {
        // call external API or local model, return PNG bytes
    }
}

// In Plugin::reload() when enabled:
ctx.image_generate_registry.register(Arc::new(MyImageGenerator { ... })).await;

// In Plugin::stop() or reload() when disabled:
ctx.image_generate_registry.unregister("my_generator").await;
```

---

## ComfyUI plugin (`crates/plugin-comfyui`)

Each JSON file in `data/comfyui/workflows/` becomes a separate `ImageGenerate`
provider. The plugin monitors ComfyUI health every 5s and unregisters all
providers if the server is unreachable.

Workflow files must be exported from ComfyUI as "API Format". The plugin reads
an optional `_personal_agent` key for metadata:

```json
"_personal_agent": {
  "name": "Realistic Portrait",
  "description": "Ritratti realistici, formato verticale. Default 768×1024.",
  "prompt_node": "6",
  "negative_prompt_node": "7",
  "prompt_field": "clip_l",
  "prompt_field_extra": ["clip_g", "t5xxl"],
  "extra_params": { "width_node": "8", "height_node": "8", "steps_node": "3" }
}
```

- `prompt_field` (optional): input field to write the prompt into. Default `"text"` (for `CLIPTextEncode`). Use `"clip_l"` for `CLIPTextEncodeSD3`.
- `prompt_field_extra` (optional): additional input fields to copy the same prompt into. For SD3.5: `["clip_g", "t5xxl"]`.
- `negative_prompt_field` / `negative_prompt_field_extra`: same for the negative prompt node.

Provider id: `comfyui-{filename}` (e.g. `realistic-portrait.json` → `comfyui-realistic-portrait`).

See [../comfyui-workflow-format.md](../comfyui-workflow-format.md) for the complete
guide on reading and modifying workflow files.

---

## When to Update This File

- A new concrete `ImageGenerate` implementation is added (e.g. a new provider backend)
- Image storage path or REST endpoint changes
- LLM tool signatures change
