mod db;
pub mod manager;
pub mod openrouter_image;

pub use core_api::image_generate::ImageGenerate;
pub use core_api::image_generate::ImageGenerateModelRecord;
pub use manager::ImageGeneratorManager;

/// Public model metadata for API responses.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageGenerateModelInfo {
    pub id:            i64,
    pub provider_id:   i64,
    pub provider_name: String,
    pub model_id:      String,
    pub name:          String,
    pub priority:      i32,
    /// `true` for plugin-registered (ephemeral) providers — not editable via the UI.
    pub from_plugin:   bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description:   Option<String>,
}

// ── Tool-facing types ─────────────────────────────────────────────────────────

/// Lightweight provider listing returned by `image_generate_providers_list`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ImageGenerateInfo {
    pub id:                  String,
    pub name:                String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description:         Option<String>,
    /// JSON Schema for the `extra_params` argument. Present only if the provider
    /// accepts provider-specific parameters (e.g. width, height, steps).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_params_schema: Option<serde_json::Value>,
}
