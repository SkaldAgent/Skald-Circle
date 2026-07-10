use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

// ── Record types (DB ↔ manager) ───────────────────────────────────────────────

/// Full model record, mirroring one row in `image_generate_models`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ImageGenerateModelRecord {
    pub id:          i64,
    pub provider_id: i64,
    pub model_id:    String,
    /// Display alias (also used as the generator `id()`).
    pub name:        String,
    /// Lower number = tried first by `get()`.
    pub priority:    i32,
}

/// Implemented by any provider that can generate an image from a text prompt.
/// Returns raw image bytes (PNG expected).
#[async_trait]
pub trait ImageGenerate: Send + Sync {
    /// A stable, unique identifier for this provider (e.g. `"comfyui-portrait"`).
    fn id(&self) -> &str;
    /// Human-readable display name.
    fn name(&self) -> &str;
    /// Human-readable description shown to the LLM in `image_generate_providers_list`.
    /// Should mention format, style, default dimensions, and ideal use cases.
    fn description(&self) -> Option<&str> { None }
    /// JSON Schema for the `extra_params` argument accepted by this provider.
    /// Returned as-is in `image_generate_providers_list` so the LLM knows what to pass.
    fn extra_params_schema(&self) -> Option<Value> { None }
    /// Generate an image. `extra_params` is the provider-specific JSON object
    /// passed by the LLM (validated against `extra_params_schema`).
    async fn generate(&self, prompt: &str, extra_params: Option<&Value>) -> Result<Vec<u8>>;
}

/// Write-side of the image generator manager: register and remove ephemeral providers.
///
/// Implemented by `ImageGeneratorManager` in the main crate. Plugins that provide
/// their own image generation (e.g. a ComfyUI plugin) use this to register providers
/// at start and unregister them at stop.
#[async_trait]
pub trait ImageGenerateRegistry: Send + Sync {
    async fn register(&self, provider: Arc<dyn ImageGenerate>);
    async fn unregister(&self, id: &str);
}
