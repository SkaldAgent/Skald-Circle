use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::core::image_generate::ImageGeneratorManager;
use crate::core::tools::{Tool, ToolCategory, ToolDescriptionLength, truncate_label, MAX_LABEL_SHORT, MAX_LABEL_FULL};

// ── image_generate_providers_list ─────────────────────────────────────────────

pub struct ImageGenerateProvidersList {
    pub mgr: Arc<ImageGeneratorManager>,
}

impl Tool for ImageGenerateProvidersList {
    fn name(&self) -> &str { "image_generate_providers_list" }
    fn category(&self) -> ToolCategory { ToolCategory::Introspection }

    fn description(&self) -> &str {
        "List all registered image generation providers. \
         Returns an array of {id, name} objects. \
         Use the id with image_generate to pick a provider."
    }

    fn parameters_schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }

    fn describe(&self, _args: &Value, _length: ToolDescriptionLength) -> String {
        "list image providers".to_string()
    }

    fn execute_async<'a>(&'a self, _args: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        let mgr = Arc::clone(&self.mgr);
        Box::pin(async move {
            let providers = mgr.list().await;
            Ok(serde_json::to_string_pretty(&providers)?)
        })
    }
}

// ── image_generate ────────────────────────────────────────────────────────────

pub struct ImageGenerateTool {
    pub mgr: Arc<ImageGeneratorManager>,
}

impl Tool for ImageGenerateTool {
    fn name(&self) -> &str { "image_generate" }
    fn category(&self) -> ToolCategory { ToolCategory::Config }

    fn description(&self) -> &str {
        "Generate an image from a text prompt. \
         Blocks until the image is ready, then returns the local path and a web URL."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["provider_id", "prompt"],
            "properties": {
                "provider_id": {
                    "type":        "string",
                    "description": "ID of the image generation provider (from image_generate_providers_list)"
                },
                "prompt": {
                    "type":        "string",
                    "description": "Text prompt describing the image to generate"
                },
                "extra_params": {
                    "type":        "object",
                    "description": "Optional provider-specific parameters (e.g. width, height, steps). \
                                    See extra_params_schema in image_generate_providers_list for valid fields."
                }
            }
        })
    }

    fn describe(&self, args: &Value, length: ToolDescriptionLength) -> String {
        let provider = args["provider_id"].as_str().unwrap_or("?");
        let prompt   = args["prompt"].as_str().unwrap_or("?");
        match length {
            ToolDescriptionLength::Short => truncate_label(&format!("generate image ({provider})"), MAX_LABEL_SHORT),
            ToolDescriptionLength::Full  => truncate_label(&format!("generate image ({provider}): {prompt}"), MAX_LABEL_FULL),
        }
    }

    fn execute_async<'a>(&'a self, args: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>> {
        let mgr = Arc::clone(&self.mgr);
        Box::pin(async move {
            let provider_id = args["provider_id"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing provider_id"))?
                .to_string();
            let prompt = args["prompt"].as_str()
                .ok_or_else(|| anyhow::anyhow!("missing prompt"))?
                .to_string();
            let extra_params = match &args["extra_params"] {
                Value::Object(_) => Some(args["extra_params"].clone()),
                _                => None,
            };
            let (path, url) = mgr.generate(&provider_id, &prompt, extra_params.as_ref()).await?;
            Ok(json!({ "path": path, "url": url }).to_string())
        })
    }
}
